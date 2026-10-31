//! Starts the health monitor: its store, its background tasks and the probing.

use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use fynd_core::simulation::{ComponentProbe, ProbeSizer};
use tokio::task::JoinHandle;
use tracing::info;
use tycho_simulation::evm::tycho_models::Chain;

use crate::{
    latest_stream::LatestStream,
    monitor::{
        component_checker::MarketComponentChecker,
        component_gauges::report_components,
        component_health::HealthThresholds,
        component_prober::ComponentProber,
        excluded_publisher::ExcludedPublisher,
        health_file,
        health_store::HealthStore,
        market_feed::start_market_feed,
        market_membership::keep_tracking,
        monitor_args::MonitorArgs,
        monitor_clock::MonitorClock,
        probe_liveness::{watch_liveness, ProbeLiveness},
        probe_scheduler::ProbeScheduler,
        Monitor,
    },
};

/// How long the market may take to receive its first block and compute the derived data.
const MARKET_READY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The longest a simulation call of one probe may take.
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the market feed is checked for having ended.
const FEED_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// A running health monitor.
pub struct LaunchedMonitor {
    /// The parts the API shares with the prober.
    pub monitor: Arc<Monitor>,
    /// Ends, with the reason, when the monitor can no longer probe: the market feed ended or
    /// could not start, or the node cannot be used.
    pub task: JoinHandle<anyhow::Result<()>>,
}

/// Starts the health monitor of `chain`, which publishes its excluded components to `stream`.
///
/// Reads the health file before returning, so a malformed file fails the start. The market feed
/// and the probing run on a task that ends when the monitor cannot continue.
///
/// # Errors
///
/// Fails when `--tycho-url` is missing, when the probe size is invalid, or when the health
/// file is malformed.
pub fn launch_monitor(
    chain: Chain,
    args: MonitorArgs,
    stream: Arc<LatestStream>,
) -> anyhow::Result<LaunchedMonitor> {
    anyhow::ensure!(
        args.tycho_url.is_some(),
        "the health monitor needs --tycho-url (TYCHO_URL) as well as --rpc-url"
    );
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
    let task = tokio::spawn(run_probing(Arc::clone(&monitor), args, sizer));
    Ok(LaunchedMonitor { monitor, task })
}

/// Starts the market feed, waits for it to be ready, and probes until the feed ends.
async fn run_probing(
    monitor: Arc<Monitor>,
    args: MonitorArgs,
    sizer: ProbeSizer,
) -> anyhow::Result<()> {
    let chain = monitor.chain;
    let feed = start_market_feed(chain, &args).await?;
    let events = feed.subscribe_market_events();
    info!("waiting for the market to be ready");
    feed.wait_until_ready(MARKET_READY_TIMEOUT)
        .await
        .context("the market did not become ready")?;
    let rpc_url = args
        .rpc_url
        .as_deref()
        .context("the health monitor needs --rpc-url")?;
    let probe = ComponentProbe::connect(chain, rpc_url, PROBE_REQUEST_TIMEOUT)
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
    let mut feed_check = tokio::time::interval(FEED_CHECK_INTERVAL);
    let ended = loop {
        tokio::select! {
            result = &mut tracking => break format!("the market tracker stopped: {result:?}"),
            result = &mut probing => break format!("the prober stopped: {result:?}"),
            _ = feed_check.tick() => {
                if feed.feed_has_ended() {
                    break "the Tycho feed ended, so the market no longer updates".to_string();
                }
            }
        }
    };
    tracking.abort();
    probing.abort();
    feed.shutdown();
    anyhow::bail!("{ended}")
}
