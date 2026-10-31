//! Starts the health monitor: its store, its background tasks and the probing.

use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use fynd_core::{
    simulation::{ComponentProbe, ProbeSizer},
    MarketFeed,
};
use tokio::task::JoinHandle;
use tracing::info;
use tycho_simulation::evm::tycho_models::Chain;

use crate::{
    latest_stream::LatestStream,
    market::market_launch::wait_until_market_ready,
    monitor::{
        component_checker::MarketComponentChecker,
        component_gauges::report_components,
        component_health::HealthThresholds,
        component_prober::ComponentProber,
        excluded_publisher::ExcludedPublisher,
        health_file,
        health_store::HealthStore,
        market_membership::keep_tracking,
        monitor_args::MonitorArgs,
        monitor_clock::MonitorClock,
        probe_liveness::{watch_liveness, ProbeLiveness},
        probe_scheduler::ProbeScheduler,
        Monitor,
    },
};

/// The longest a simulation call of one probe may take.
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A running health monitor.
pub struct LaunchedMonitor {
    /// The parts the API shares with the prober.
    pub monitor: Arc<Monitor>,
    /// Ends, with the reason, when the monitor can no longer probe: the market did not become
    /// ready, the node cannot be used, or a part of the monitor stopped.
    pub task: JoinHandle<anyhow::Result<()>>,
}

/// Starts the health monitor of `chain`, which probes the components of `feed` through the node
/// at `rpc_url` and publishes its excluded components to `stream`.
///
/// Reads the health file before returning, so a malformed file fails the start. The probing runs
/// on a task that ends when the monitor cannot continue.
///
/// # Errors
///
/// Fails when the probe size is invalid or when the health file is malformed.
pub fn launch_monitor(
    chain: Chain,
    args: MonitorArgs,
    feed: Arc<MarketFeed>,
    rpc_url: String,
    stream: Arc<LatestStream>,
) -> anyhow::Result<LaunchedMonitor> {
    let sizer =
        ProbeSizer::new(args.probe_usd, args.probe_depth_share).context("invalid probe size")?;
    let records = health_file::load(&args.health_file)?;
    info!(path = ?args.health_file, count = records.len(), "loaded the health records");
    let thresholds = HealthThresholds {
        broken_after: args.broken_after,
        inaccurate_after: args.inaccurate_after,
        inaccuracy_bps: args.inaccuracy_bps,
    };
    let clock = MonitorClock::new();
    let store = Arc::new(HealthStore::new(thresholds, records));
    let monitor = Arc::new(Monitor {
        chain,
        scheduler: ProbeScheduler::new(args.refresh_interval, args.max_retry_backoff),
        publisher: Arc::new(ExcludedPublisher::new(stream, Arc::clone(&store), clock)),
        liveness: Arc::new(ProbeLiveness::new(args.probe_interval())),
        store,
        clock,
    });
    // Pods fail open on a 204, so the saved exclusions are served while the market loads
    // rather than after the first new verdict.
    if monitor.store.has_verdicts() {
        monitor.publisher.sync();
    }
    tokio::spawn(health_file::keep_saved(Arc::clone(&monitor.store), args.health_file.clone()));
    tokio::spawn(watch_liveness(
        Arc::clone(&monitor.liveness),
        Arc::clone(&monitor.publisher),
        chain.to_string(),
    ));
    tokio::spawn(report_components(Arc::clone(&monitor.store), chain.to_string()));
    let task = tokio::spawn(run_probing(Arc::clone(&monitor), args, sizer, feed, rpc_url));
    Ok(LaunchedMonitor { monitor, task })
}

/// Waits for the market to be ready, then probes its components until a part stops.
async fn run_probing(
    monitor: Arc<Monitor>,
    args: MonitorArgs,
    sizer: ProbeSizer,
    feed: Arc<MarketFeed>,
    rpc_url: String,
) -> anyhow::Result<()> {
    let chain = monitor.chain;
    let events = feed.subscribe_market_events();
    wait_until_market_ready(&feed).await?;
    let probe = ComponentProbe::connect(chain, &rpc_url, PROBE_REQUEST_TIMEOUT)
        .await
        .context("cannot connect the component probe")?;
    let checker =
        MarketComponentChecker::new(feed.market_data(), feed.derived_data(), sizer, probe);
    let mut tracking = tokio::spawn(keep_tracking(
        feed.market_data(),
        events,
        Arc::clone(&monitor.store),
        monitor.clock,
    ));
    let prober = ComponentProber::new(checker, Arc::clone(&monitor), args.probe_interval());
    let mut probing = tokio::spawn(prober.run());
    info!(interval = ?args.probe_interval(), "probing the components");
    let ended = tokio::select! {
        result = &mut tracking => format!("the market tracker stopped: {result:?}"),
        result = &mut probing => format!("the prober stopped: {result:?}"),
    };
    tracking.abort();
    probing.abort();
    anyhow::bail!("{ended}")
}
