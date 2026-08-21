//! Building and rebuilding the in-process stepped solver that both live commands drive.
//!
//! `monitor` and `decay` need the same thing: an in-process `fynd-core` solver whose block stream
//! is gated by a [`BlockStepController`], rebuilt in place when the Tycho feed dies so a long
//! unattended run survives feed failures. That construction — protocol expansion, worker-pool
//! loading, build, rebuild-with-backoff — lives here so neither command owns a private copy.

use std::{future::Future, path::Path, pin::Pin, time::Duration};

use alloy::providers::Provider;
use async_trait::async_trait;
use fynd_core::{BlockStepController, FyndBuilder, Solver};
use tracing::{info, warn};
use tycho_simulation::tycho_common::models::Chain;

/// Pause between solver rebuild attempts after a feed death, so a struggling Tycho server is not
/// hammered in a tight loop.
const REBUILD_BACKOFF: Duration = Duration::from_secs(30);

/// Wall-clock budget behind chain head that [`default_lag_blocks`] converts into a block count.
const LAG_BUDGET_SECS: u64 = 20 * 60;

/// Block time assumed for a custom chain with no registered one.
const FALLBACK_BLOCK_TIME_SECS: u64 = 12;

/// Everything needed to build the in-process stepped solver, shared by the commands that drive one.
///
/// Flattened into each command's own args, so `monitor` and `decay` expose an identical solver
/// surface and a run of one can be reproduced by the other with the same flags.
#[derive(clap::Args)]
pub(crate) struct SolverArgs {
    #[command(flatten)]
    pub chain: crate::ChainArgs,

    /// Tycho WebSocket URL feeding the in-process solver
    #[arg(long, env = "TYCHO_URL")]
    pub tycho_url: String,

    /// Protocols to index, comma-separated. Defaults to every native on-chain protocol; use
    /// `all_onchain` to include VM-simulated ones too (see `fynd serve --protocols`)
    #[arg(long, value_delimiter = ',', default_value = "native_onchain")]
    pub protocols: Vec<String>,

    /// Minimum pool TVL filter for the solver
    #[arg(long, default_value_t = 100.0)]
    pub min_tvl: f64,

    /// Tycho API key (if the endpoint requires one)
    #[arg(long, env = "TYCHO_API_KEY")]
    pub tycho_api_key: Option<String>,

    /// Worker-pools TOML config (algorithm/hops/workers); the default path falls back to Fynd's
    /// built-in default pools when absent, like `fynd serve`. Custom paths that don't exist fail
    /// fast
    #[arg(long, env = "WORKER_POOLS_CONFIG", default_value = "worker_pools.toml")]
    pub worker_pools_config: std::path::PathBuf,

    /// Per-quote timeout in milliseconds. Defaults to the budget `fynd serve` gives a real quote,
    /// because a measurement only means "what would Fynd have returned" if Fynd is given the same
    /// time it would have had in production. A request-level timeout overrides the router's
    /// default outright (see `WorkerPoolRouter::effective_timeout`), so a generous value here
    /// silently hands the solve more time than any production quote gets — and, on a sub-second
    /// chain, lets one solve outlast several blocks.
    #[arg(long, default_value_t = fynd_rpc::config::defaults::WORKER_ROUTER_TIMEOUT_MS)]
    pub timeout_ms: u64,

    /// Serve Prometheus metrics on this port
    #[arg(long)]
    pub metrics_port: Option<u16>,

    /// Stop after this many blocks (runs until interrupted if omitted)
    #[arg(long)]
    pub max_blocks: Option<u64>,

    /// Chain-head lag (in blocks) beyond which the session is considered unhealthy and the solver
    /// is rebuilt — seen live: a worker died, every solve crawled, and the run slid hours behind
    /// while the feed-dead watchdog never fired (blocks still trickled through). When omitted,
    /// defaults to roughly 20 minutes' worth of blocks for the chain's block time
    #[arg(long)]
    pub max_lag_blocks: Option<u64>,
}

/// The chain's block time, which every pacing budget is expressed against. A custom chain with no
/// registered block time falls back to 12-second blocks.
pub(crate) fn block_time(chain: Chain) -> Duration {
    let secs = chain
        .try_block_time_secs()
        .unwrap_or(FALLBACK_BLOCK_TIME_SECS)
        .max(1);
    Duration::from_secs(secs)
}

/// The default `--max-lag-blocks`: a ~20-minute wall-clock budget for how far behind chain head a
/// run may fall before rebuilding, expressed as a block count at the chain's block time so the
/// budget stays about the same wall-clock length on every chain.
pub(crate) fn default_lag_blocks(chain: Chain) -> u64 {
    (LAG_BUDGET_SECS / block_time(chain).as_secs()).max(1)
}

/// Resolves when the process receives Ctrl-C (SIGINT), the signal a live run treats as "stop".
/// If the handler cannot be installed the future never resolves, so a failed registration disables
/// graceful shutdown rather than tearing the run down immediately.
pub(crate) async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!(error = %e, "failed to install Ctrl-C handler; graceful shutdown disabled");
        std::future::pending::<()>().await;
    }
}

/// A prepared solver factory: protocols expanded against Tycho and the worker-pool config loaded
/// once, so every build and rebuild in a run uses identical inputs.
pub(crate) struct Session {
    args: SolverArgs,
    chain: Chain,
    protocols: Vec<String>,
    pools_config: fynd_rpc::config::WorkerPoolsConfig,
}

impl Session {
    /// Expand the protocol list against Tycho and load the worker-pool config, like `fynd serve`.
    pub(crate) async fn prepare(args: SolverArgs) -> anyhow::Result<Self> {
        let chain = args.chain.chain()?;

        // Expand protocol tokens (e.g. `native_onchain`/`all_onchain`) against Tycho, like
        // serve/scale.
        let protocols = fynd_rpc::protocols::resolve_protocols(
            &args.tycho_url,
            args.tycho_api_key.as_deref(),
            true,
            chain,
            &args.protocols,
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to resolve protocols: {e}"))?;

        let pools_config = load_pools_config(&args.worker_pools_config)?;
        Ok(Self { args, chain, protocols, pools_config })
    }

    pub(crate) fn args(&self) -> &SolverArgs {
        &self.args
    }

    pub(crate) fn chain(&self) -> Chain {
        self.chain
    }

    /// Build the in-process solver and its block-step controller.
    pub(crate) async fn build(&self) -> anyhow::Result<(Solver, BlockStepController)> {
        info!(
            chain = self.args.chain.name,
            protocols = self.protocols.len(),
            "building in-process solver (loading tokens may take minutes)…"
        );
        let mut builder = FyndBuilder::new(
            self.chain,
            &self.args.tycho_url,
            &self.args.chain.rpc_url,
            self.protocols.clone(),
            self.args.min_tvl,
        );
        if let Some(key) = self.args.tycho_api_key.as_deref() {
            builder = builder.tycho_api_key(key);
        }
        for (name, pool) in self.pools_config.pools() {
            builder = builder
                .add_pool(name, pool)
                .map_err(|e| anyhow::anyhow!("failed to add worker pool {name}: {e}"))?;
        }
        builder
            .build_with_step_controller()
            .await
            .map_err(|e| anyhow::anyhow!("failed to build solver: {e}"))
    }

    /// Rebuild the solver after a feed death, retrying with backoff until it succeeds. Returns
    /// `None` when `shutdown` resolves first (Ctrl-C during the retry loop or a build), so the
    /// caller stops instead of rebuilding.
    pub(crate) async fn rebuild<S: Future<Output = ()>>(
        &self,
        mut shutdown: Pin<&mut S>,
    ) -> Option<(Solver, BlockStepController)> {
        loop {
            let rebuilt = tokio::select! {
                biased;
                () = shutdown.as_mut() => return None,
                result = async {
                    tokio::time::sleep(REBUILD_BACKOFF).await;
                    self.build().await
                } => result,
            };
            match rebuilt {
                Ok(built) => return Some(built),
                Err(e) => warn!(error = %e, "solver rebuild failed; retrying"),
            }
        }
    }
}

/// A source of the current chain head, abstracted so a lag check stays testable without a live
/// RPC connection.
#[async_trait]
pub(crate) trait HeadSource: Send + Sync {
    /// The chain's current head block, or `None` when it could not be determined — treated as
    /// "skip this check" rather than as a lag, since a flaky RPC call is not evidence the session
    /// fell behind.
    async fn head(&self) -> Option<u64>;
}

/// Reads the chain head from a live JSON-RPC provider.
pub(crate) struct RpcHead<P>(P);

impl<P> RpcHead<P> {
    pub(crate) fn new(provider: P) -> Self {
        Self(provider)
    }
}

#[async_trait]
impl<P: Provider + Send + Sync> HeadSource for RpcHead<P> {
    async fn head(&self) -> Option<u64> {
        self.0.get_block_number().await.ok()
    }
}

/// Chain-head lag guard: how far behind head a session may fall before it is considered unhealthy
/// and its solver rebuilt. `monitor` has its own inline version of this check, folded into its
/// per-block `Pacing`; this one is `decay`'s (see `decay::run_session`), kept here so neither
/// reinvents the head-polling and telemetry plumbing.
pub(crate) struct LagGuard {
    source: Box<dyn HeadSource>,
    budget: u64,
}

impl LagGuard {
    /// `max_lag_blocks`, or the chain's ~20-minute default when omitted.
    pub(crate) fn new(
        source: impl HeadSource + 'static,
        chain: Chain,
        max_lag_blocks: Option<u64>,
    ) -> Self {
        Self {
            source: Box::new(source),
            budget: max_lag_blocks.unwrap_or_else(|| default_lag_blocks(chain)),
        }
    }

    /// `target`'s lag behind the current chain head, recorded to telemetry regardless of whether
    /// it trips the budget. `Some((head, lag))` only when the lag exceeds the budget — the
    /// trigger to consider the session unhealthy. `None` on a transient head-lookup failure too:
    /// a confirmed lag is worth a solver rebuild, a flaky lookup is not.
    pub(crate) async fn exceeded(&self, target: u64) -> Option<(u64, u64)> {
        let head = self.source.head().await?;
        let lag = head.saturating_sub(target);
        crate::telemetry::record_head_lag_blocks(lag);
        (lag > self.budget).then_some((head, lag))
    }
}

/// Load worker pools like `fynd serve`: the default path falls back to the built-in default pools
/// when absent; a custom path that does not exist fails fast.
fn load_pools_config(path: &Path) -> anyhow::Result<fynd_rpc::config::WorkerPoolsConfig> {
    let default_path = Path::new("worker_pools.toml");
    if path == default_path && !default_path.exists() {
        info!("worker_pools.toml not found; using Fynd's built-in default pools");
        return Ok(fynd_rpc::config::WorkerPoolsConfig::builtin_default());
    }
    fynd_rpc::config::WorkerPoolsConfig::load_from_file(path)
        .map_err(|e| anyhow::anyhow!("failed to load worker pools config {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_lag_blocks_scales_with_block_time() {
        assert_eq!(default_lag_blocks(Chain::Ethereum), 100); // 12s blocks
        assert_eq!(default_lag_blocks(Chain::Base), 600); // 2s blocks
        assert_eq!(default_lag_blocks(Chain::Unichain), 1200); // 1s blocks
    }

    #[test]
    fn test_block_time_per_chain() {
        assert_eq!(block_time(Chain::Ethereum), Duration::from_secs(12));
        assert_eq!(block_time(Chain::Base), Duration::from_secs(2));
    }

    #[test]
    fn test_missing_default_pools_config_falls_back_to_builtin() {
        // A non-default path that does not exist must fail fast rather than silently defaulting.
        assert!(load_pools_config(Path::new("definitely-not-here.toml")).is_err());
    }

    struct FixedHead(Option<u64>);

    #[async_trait]
    impl HeadSource for FixedHead {
        async fn head(&self) -> Option<u64> {
            self.0
        }
    }

    #[tokio::test]
    async fn test_lag_guard_is_quiet_within_budget() {
        let guard = LagGuard::new(FixedHead(Some(105)), Chain::Ethereum, Some(10));
        assert_eq!(guard.exceeded(100).await, None);
    }

    #[tokio::test]
    async fn test_lag_guard_trips_past_budget() {
        let guard = LagGuard::new(FixedHead(Some(120)), Chain::Ethereum, Some(10));
        assert_eq!(guard.exceeded(100).await, Some((120, 20)));
    }

    #[tokio::test]
    async fn test_lag_guard_defaults_to_the_chain_budget_when_unset() {
        // Ethereum's default is 100 blocks (~20 minutes at 12s blocks): 90 blocks behind is
        // within it, 110 is not.
        let guard = LagGuard::new(FixedHead(Some(1_090)), Chain::Ethereum, None);
        assert_eq!(guard.exceeded(1_000).await, None);
        let guard = LagGuard::new(FixedHead(Some(1_110)), Chain::Ethereum, None);
        assert_eq!(guard.exceeded(1_000).await, Some((1_110, 110)));
    }

    #[tokio::test]
    async fn test_lag_guard_treats_an_unknown_head_as_not_exceeded() {
        // A transient head-lookup failure is not evidence of a lag — only a confirmed one is
        // worth the cost of a solver rebuild.
        let guard = LagGuard::new(FixedHead(None), Chain::Ethereum, Some(0));
        assert_eq!(guard.exceeded(100).await, None);
    }
}
