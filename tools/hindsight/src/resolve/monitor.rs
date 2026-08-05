//! Live two-state monitor: drive an in-process `fynd-core` solver one block at a time, solving
//! each block's settled trades at top-of-block (N-1) and measuring twice at back-of-block (N) —
//! each top route is re-executed to isolate slippage between quote time and execution time, and
//! each trade is solved fresh to show what routing at the block's end state would deliver.
//!
//! The block barrier is deterministic: after releasing a block via
//! `BlockStepController::trigger_next_block`, we wait until the solver's `MarketData` reports the
//! next applied block before re-solving back-of-block. The pure orchestration is unit-tested in the
//! parent module via a mock `SteppingSolver`; this live driver is exercised by the gated
//! integration test in `tests/` (requires `TYCHO_URL` + `RPC_URL`).

use std::{
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use async_trait::async_trait;
use fynd_core::{
    types::{
        EncodingOptions, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest, QuoteStatus,
    },
    BlockStepController, FyndBuilder, Solver,
};
use num_bigint::BigUint;
use tracing::{debug, info, warn};
use tycho_simulation::tycho_common::models::{Address as CoreAddress, Chain};

use crate::{
    decoder::{DecodedTrade, Decoder, Registry},
    provider_from,
    resolve::{solve_backs, solve_tops, Outcome, SolvedAmount, SteppingSolver},
    telemetry,
    usd::Prices,
};

/// How often to warn while the solver has not applied the next block.
const STALL_WARN_INTERVAL: Duration = Duration::from_mins(5);
/// No block for this long means the feed is dead, not slow. The observed failure mode: one
/// server-side subscription goes silent, tycho-client's block synchronizer stops emitting while
/// it waits for it, and ~35 minutes later backpressure kills the remaining subscriptions
/// ("Buffer full, unsubscribing!"). Nothing resubscribes, so the stream never recovers — the
/// monitor rebuilds the solver instead of waiting.
const FEED_DEAD_TIMEOUT: Duration = Duration::from_mins(15);
/// Pause between solver rebuild attempts after a feed death, so a struggling Tycho server is not
/// hammered in a tight loop.
const REBUILD_BACKOFF: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How far the receipts RPC may trail the Tycho stream before the monitor decodes the target block
/// anyway. In blocks, not seconds, because an RPC node trails by blocks.
const DECODE_RPC_LAG_BUDGET_BLOCKS: u32 = 3;
/// How often to re-check the RPC head while waiting out that lag. Well under a block time: the
/// monitor only idles until the block lands, and `eth_blockNumber` is cheap.
const DECODE_RPC_LAG_POLL: Duration = Duration::from_millis(250);

/// Wall-clock budget behind chain head that `default_lag_blocks` converts into a block count.
const LAG_BUDGET_SECS: u64 = 20 * 60;

/// Inputs for the live monitor — the `monitor` subcommand's CLI arguments, used directly as its
/// configuration.
#[derive(clap::Args)]
pub(crate) struct MonitorArgs {
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
    /// because the comparison only means "what would Fynd have returned" if Fynd is given the same
    /// time it would have had in production. A request-level timeout overrides the router's
    /// default outright (see `WorkerPoolRouter::effective_timeout`), so a generous value here
    /// silently hands the re-solve more time than any production quote gets — overstating
    /// savings — and, on a sub-second chain, lets one solve outlast several blocks.
    #[arg(long, default_value_t = fynd_rpc::config::defaults::WORKER_ROUTER_TIMEOUT_MS)]
    pub timeout_ms: u64,

    /// Serve Prometheus metrics on this port
    #[arg(long)]
    pub metrics_port: Option<u16>,

    /// Stop after this many blocks (runs until interrupted if omitted)
    #[arg(long)]
    pub max_blocks: Option<u64>,

    /// Chain-head lag (in blocks) beyond which the session is considered unhealthy and the solver
    /// is rebuilt — seen live: a worker died, every solve crawled, and the monitor slid hours
    /// behind while the feed-dead watchdog never fired (blocks still trickled through). When
    /// omitted, defaults to roughly 20 minutes' worth of blocks for the chain's block time
    #[arg(long)]
    pub max_lag_blocks: Option<u64>,

    /// Write one JSON line per re-solved trade (every comparison — wins, losses, and unsolvable
    /// coverage gaps) into this directory as `comparisons-YYYY-MM-DD.jsonl`, rotated at each UTC
    /// day boundary so an external sync job can ship closed daily files. Each record carries
    /// both block states with verdict, bps, USD delta, and a slim route/calldata or unsolvable
    /// reason; filter downstream for the improvement or coverage view
    #[arg(long)]
    pub comparisons_dir: Option<std::path::PathBuf>,

    /// Write one JSON line per block into this directory as `batches-YYYY-MM-DD.jsonl` (same
    /// daily rotation as `--comparisons-dir`): every decoded trade as an APEX batch order, with
    /// its limit, its Fynd top-of-block counterfactual, and the solver's token-price map at N-1.
    /// Joined offline against a `record-market` recording by `tools/apex-batch`, which replays
    /// the batch and measures the surplus batch clearing would have delivered over Fynd's
    /// per-order baseline
    #[arg(long)]
    pub capture_dir: Option<std::path::PathBuf>,

    /// Solve each eligible block's trades as an APEX batch, at both bracket states (N-1 and N),
    /// off the block loop's critical path. Results land as `apex-YYYY-MM-DD.jsonl` in this
    /// directory (same daily rotation as the other writers), joinable against the comparisons
    /// records on `{tx_hash}:{tx_index}`
    #[arg(long)]
    pub apex_dir: Option<std::path::PathBuf>,

    /// APEX worker threads (the stage's whole CPU ceiling — dedicated OS threads, not tokio's
    /// blocking pool)
    #[arg(long, default_value_t = 2)]
    pub apex_workers: usize,

    /// Bounded APEX job queue; a full queue sheds the block's job (counted) instead of stalling
    #[arg(long, default_value_t = 8)]
    pub apex_queue_capacity: usize,

    /// APEX search budget per component solve, in milliseconds (the study's live budget)
    #[arg(long, default_value_t = 1_000)]
    pub apex_budget_ms: u64,

    /// Search budget per single-order control solve, in milliseconds
    #[arg(long, default_value_t = 250)]
    pub apex_single_budget_ms: u64,

    /// Pool-subset cap per batch (native-only 2-hop closure, class-ordered)
    #[arg(long, default_value_t = 400)]
    pub apex_max_pools: usize,
}

/// Drives the in-process solver, stepping the chain one block per `SteppingSolver::advance`.
struct StepAdapter<'a> {
    solver: &'a Solver,
    controller: &'a BlockStepController,
    timeout_ms: u64,
}

impl StepAdapter<'_> {
    /// The block number of the solver's currently-applied market state, if any.
    async fn current_block(&self) -> Option<u64> {
        self.solver
            .market_data()
            .read()
            .await
            .last_updated()
            .map(fynd_core::BlockInfo::number)
    }
}

#[async_trait]
impl SteppingSolver for StepAdapter<'_> {
    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome {
        let Ok(amount) = amount_in.to_string().parse::<BigUint>() else {
            return Outcome::Unsolvable("unparseable amount_in".to_string());
        };
        // Placeholder receiver: routing/amounts are receiver-independent; it only fills the encoded
        // calldata's recipient. Encoding is requested so each quote carries its on-chain
        // transaction (note: this refines gas estimates and a failed encode yields
        // Unsolvable).
        let order = Order::new(
            CoreAddress::from(token_in.into_array()),
            CoreAddress::from(token_out.into_array()),
            amount,
            OrderSide::Sell,
            CoreAddress::from([0x11u8; 20]),
        );
        let request = QuoteRequest::new(
            vec![order],
            QuoteOptions::default()
                .with_timeout_ms(self.timeout_ms)
                .with_encoding_options(EncodingOptions::new(0.005)),
        );

        let quote = match self.solver.quote(request).await {
            Ok(quote) => quote,
            Err(e) => return Outcome::Unsolvable(format!("solve error: {e}")),
        };
        let Some(order) = quote.orders().first() else {
            return Outcome::Unsolvable("solver returned no order quote".to_string());
        };
        order_quote_to_outcome(order)
    }

    async fn reexecute(&self, top: &SolvedAmount) -> Outcome {
        let Some(route) = top.solved_route.as_ref() else {
            return Outcome::Unsolvable("top-of-block quote carried no route".to_string());
        };
        let market = self.solver.market_data();
        let view = market.read().await;
        match fynd_core::replay_route(route, view.base_market_state()) {
            Ok(replay) => {
                let amount_out = biguint_to_u256(&replay.amount_out);
                // Same route ⇒ same gas: reuse the top quote's gas deduction (in token_out
                // units) and its encoding-refined gas estimate instead of re-deriving gas
                // prices at the new block state.
                let gas_deduction = top
                    .amount_out
                    .saturating_sub(top.amount_out_net_gas);
                Outcome::Solved(SolvedAmount {
                    amount_out,
                    amount_out_net_gas: amount_out.saturating_sub(gas_deduction),
                    gas_estimate: top.gas_estimate,
                    // Same route re-executed: attribution carries over from the top quote. The
                    // route itself does not — nothing serializes a re-executed outcome's route
                    // (it only feeds the slippage numbers via its amounts).
                    algorithm: top.algorithm.clone(),
                    quote_json: top.quote_json.clone(),
                    solved_route: None,
                })
            }
            Err(e) => Outcome::Unsolvable(format!("re-execution failed: {e}")),
        }
    }

    async fn advance(&self) -> anyhow::Result<()> {
        let before = self.current_block().await;
        self.controller
            .trigger_next_block()
            .map_err(|_| anyhow::anyhow!("tycho stream ended (trigger channel closed)"))?;

        // Deterministic barrier: wait until the solver applies a block strictly newer than
        // `before`. An error here means the feed died — either its stream ended (peek returns
        // None once the gating task exits) or it jammed without ending (no block within
        // FEED_DEAD_TIMEOUT). The caller rebuilds the solver on any error.
        let stall_started = Instant::now();
        let mut next_warn = stall_started + STALL_WARN_INTERVAL;
        loop {
            if let Some(now) = self.current_block().await {
                if before.is_none_or(|b| now > b) {
                    return Ok(());
                }
            }
            if stall_started.elapsed() >= FEED_DEAD_TIMEOUT {
                anyhow::bail!(
                    "no block applied in {}s; tycho feed presumed dead",
                    stall_started.elapsed().as_secs()
                );
            }
            if Instant::now() >= next_warn {
                warn!(
                    waited_s = stall_started.elapsed().as_secs(),
                    last_applied_block = ?before,
                    "tycho stream stalled; waiting for the next block"
                );
                next_warn += STALL_WARN_INTERVAL;
            }
            tokio::select! {
                () = tokio::time::sleep(POLL_INTERVAL) => {}
                peeked = self.controller.peek_next_block() => {
                    if peeked.is_none() {
                        anyhow::bail!("tycho stream ended while waiting for the next block");
                    }
                }
            }
        }
    }
}

fn order_quote_to_outcome(quote: &OrderQuote) -> Outcome {
    if quote.status() != QuoteStatus::Success {
        return Outcome::Unsolvable(format!("{:?}", quote.status()));
    }
    // Project the quote to a slim route + calldata, built directly from the quote object. We must
    // NOT serialize the whole `OrderQuote`: it embeds each hop's `protocol_state`, which both
    // dominates size and fails to serialize for vm pools (e.g. Curve) — dropping the entire route
    // for exactly the deep-liquidity stable trades we care about.
    let quote_json = serde_json::to_string(&super::jsonl::slim_quote(quote)).ok();
    Outcome::Solved(SolvedAmount {
        amount_out: biguint_to_u256(quote.amount_out()),
        amount_out_net_gas: biguint_to_u256(quote.amount_out_net_gas()),
        gas_estimate: biguint_to_u256(quote.gas_estimate()),
        // Which algorithm won the quote — the winning quote is the one the `WorkerPoolRouter`
        // ranked first across every configured pool, so this is the pool that beat the others on
        // this order. The readable path is derived from `solved_route` at serialization time
        // (see `resolve::render_route`), not stored here.
        algorithm: quote.algorithm().to_string(),
        quote_json,
        // Kept in memory so the route can be re-executed at back-of-block.
        solved_route: quote.route().cloned().map(Box::new),
    })
}

fn biguint_to_u256(value: &BigUint) -> U256 {
    // Convert via big-endian bytes: avoids a decimal string round-trip and catches overflow
    // without relying on parse. U256 fits in 32 bytes; a larger value is a solver bug.
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        warn!(bits = value.bits(), "solver quote amount overflows U256; treating as zero");
        return U256::ZERO;
    }
    let mut buf = [0u8; 32];
    buf[32 - bytes.len()..].copy_from_slice(&bytes);
    U256::from_be_bytes(buf)
}

/// Decode `block`, first waiting out any RPC lag. The HTTP RPC used for receipts can trail the
/// Tycho stream that drives `block`, so poll the RPC head until it reaches `block` or `budget`
/// expires — that distinguishes a transient race from a real failure. A block still undecodable
/// once the RPC has indexed it is a genuine error and surfaces to the caller.
async fn decode_block_when_available<P: Provider>(
    decoder: &mut Decoder<P>,
    block: u64,
    budget: Duration,
) -> anyhow::Result<Vec<DecodedTrade>> {
    let started = Instant::now();
    let mut logged_wait = false;
    loop {
        let head = match decoder
            .provider()
            .get_block_number()
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(block, "failed to fetch RPC block number: {e}");
                0
            }
        };
        if head >= block {
            break;
        }
        // Once per block, not once per poll: at this cadence a per-poll line would bury the log.
        if !logged_wait {
            logged_wait = true;
            debug!(block, head, "RPC lags the tycho stream; waiting for it to index the block");
        }
        if started.elapsed() >= budget {
            warn!(
                block,
                head,
                waited_ms = started.elapsed().as_millis(),
                "RPC never indexed the block within its lag budget; decoding anyway"
            );
            break;
        }
        tokio::time::sleep(DECODE_RPC_LAG_POLL).await;
    }
    telemetry::record_rpc_index_wait(started.elapsed());
    decoder.decode_block(block).await
}

/// One session's terminal condition: the run completed (`--max-blocks` reached) or the session
/// is unhealthy — the feed died, or the monitor fell too far behind head — and the caller should
/// rebuild the solver.
enum SessionEnd {
    Complete,
    Unhealthy(String),
}

/// Counters that survive feed rebuilds, so `--max-blocks` and skip logging span the whole run.
#[derive(Default)]
struct Totals {
    processed: u64,
    skipped_blocks: u64,
}

/// Build the in-process solver and its block-step controller.
async fn build_solver(
    cfg: &MonitorArgs,
    chain: Chain,
    protocols: &[String],
    pools_config: &fynd_rpc::config::WorkerPoolsConfig,
) -> anyhow::Result<(Solver, BlockStepController)> {
    info!(
        chain = cfg.chain.name,
        protocols = protocols.len(),
        "building in-process solver (loading tokens may take minutes)…"
    );
    let mut builder = FyndBuilder::new(
        chain,
        &cfg.tycho_url,
        &cfg.chain.rpc_url,
        protocols.to_vec(),
        cfg.min_tvl,
    );
    if let Some(key) = cfg.tycho_api_key.as_deref() {
        builder = builder.tycho_api_key(key);
    }
    for (name, pool) in pools_config.pools() {
        builder = builder
            .add_pool(name, pool)
            .map_err(|e| anyhow::anyhow!("failed to add worker pool {name}: {e}"))?;
    }
    builder
        .build_with_step_controller()
        .await
        .map_err(|e| anyhow::anyhow!("failed to build solver: {e}"))
}

/// The chain's block time, which every pacing budget in the monitor is expressed against. A custom
/// chain with no registered block time falls back to 12-second blocks.
fn block_time(chain: Chain) -> Duration {
    let secs = chain
        .try_block_time_secs()
        .unwrap_or(12)
        .max(1);
    Duration::from_secs(secs)
}

/// The default `--max-lag-blocks`: a ~20-minute wall-clock budget for how far behind chain head the
/// monitor may fall before rebuilding, expressed as a block count at the chain's block time so the
/// budget stays about the same wall-clock length on every chain.
fn default_lag_blocks(chain: Chain) -> u64 {
    (LAG_BUDGET_SECS / block_time(chain).as_secs()).max(1)
}

/// The pacing budgets one session runs against, both scaled to the chain's block time.
struct Pacing {
    /// Chain-head lag beyond which the session is unhealthy and the solver is rebuilt.
    max_lag_blocks: u64,
    /// How long to wait for the receipts RPC to index the target block before decoding regardless.
    rpc_lag_budget: Duration,
}

impl Pacing {
    fn for_chain(chain: Chain, max_lag_blocks: Option<u64>) -> Self {
        Self {
            max_lag_blocks: max_lag_blocks.unwrap_or_else(|| default_lag_blocks(chain)),
            rpc_lag_budget: block_time(chain) * DECODE_RPC_LAG_BUDGET_BLOCKS,
        }
    }
}

/// Resolves when the process receives Ctrl-C (SIGINT), the signal the monitor treats as "stop".
/// If the handler cannot be installed the future never resolves, so a failed registration disables
/// graceful shutdown rather than tearing the run down immediately.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!(error = %e, "failed to install Ctrl-C handler; graceful shutdown disabled");
        std::future::pending::<()>().await;
    }
}

/// Rebuild the solver after a feed death, retrying with backoff until it succeeds. Returns `None`
/// when `shutdown` resolves first (Ctrl-C during the retry loop or a build), so the caller stops
/// instead of rebuilding.
async fn rebuild_after_feed_death<S: Future<Output = ()>>(
    cfg: &MonitorArgs,
    chain: Chain,
    protocols: &[String],
    pools_config: &fynd_rpc::config::WorkerPoolsConfig,
    mut shutdown: Pin<&mut S>,
) -> Option<(Solver, BlockStepController)> {
    loop {
        let rebuilt = tokio::select! {
            biased;
            () = shutdown.as_mut() => return None,
            result = async {
                tokio::time::sleep(REBUILD_BACKOFF).await;
                build_solver(cfg, chain, protocols, pools_config).await
            } => result,
        };
        match rebuilt {
            Ok(built) => return Some(built),
            Err(e) => warn!(error = %e, "solver rebuild failed; retrying"),
        }
    }
}

/// Build the in-process stepped solver and re-solve each block's settled trades as a top/back
/// range. When the tycho feed dies (its stream ends, or no block arrives within
/// `FEED_DEAD_TIMEOUT`), the solver is torn down and rebuilt in place — fresh subscriptions,
/// same decoder cache and comparisons file — so a long unattended run survives feed failures.
/// Ctrl-C stops the run cleanly at any await point, tearing the current solver down before
/// returning.
pub(crate) async fn run(cfg: MonitorArgs) -> anyhow::Result<()> {
    let chain = cfg.chain.chain()?;

    // Expand protocol tokens (e.g. `native_onchain`/`all_onchain`) against Tycho, like serve/scale.
    let protocols = fynd_rpc::protocols::resolve_protocols(
        &cfg.tycho_url,
        cfg.tycho_api_key.as_deref(),
        true,
        chain,
        &cfg.protocols,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to resolve protocols: {e}"))?;

    let pools_config = load_pools_config(&cfg)?;

    let mut decoder = Decoder::new(provider_from(&cfg.chain.rpc_url)?, cfg.chain.load_registry()?);

    if let Some(port) = cfg.metrics_port {
        telemetry::install_exporter(port)?;
        info!(port, "serving Prometheus metrics at /metrics");
    }

    let mut sinks = Sinks::from_args(&cfg)?;

    let mut totals = Totals::default();
    let pacing = Pacing::for_chain(chain, cfg.max_lag_blocks);
    info!(
        max_lag_blocks = pacing.max_lag_blocks,
        rpc_lag_budget_ms = pacing.rpc_lag_budget.as_millis(),
        "chain pacing budgets"
    );

    // Resolves on Ctrl-C, so a long run stops cleanly at any await below — including the
    // multi-minute solver builds. On shutdown the in-flight block is abandoned, the solver's
    // workers and background tasks are torn down, and `comparisons` flushes as it drops.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // The first build fails fast — an error here is a configuration problem. Rebuilds after a
    // feed death retry forever, since the config is known good and failures are transient.
    let (mut solver, mut controller) = tokio::select! {
        biased;
        () = &mut shutdown => return Ok(()),
        built = build_solver(&cfg, chain, &protocols, &pools_config) => built?,
    };
    loop {
        let adapter =
            StepAdapter { solver: &solver, controller: &controller, timeout_ms: cfg.timeout_ms };
        let reason = tokio::select! {
            biased;
            () = &mut shutdown => {
                info!("received Ctrl-C; shutting down");
                break;
            }
            end = run_session(
                &cfg,
                &pacing,
                &adapter,
                &mut decoder,
                &mut sinks,
                &mut totals,
            ) => match end {
                SessionEnd::Complete => break,
                SessionEnd::Unhealthy(reason) => reason,
            },
        };
        warn!(reason, "session unhealthy; rebuilding the solver to resubscribe");
        telemetry::record_feed_rebuild();
        solver.shutdown();
        let Some(built) =
            rebuild_after_feed_death(&cfg, chain, &protocols, &pools_config, shutdown.as_mut())
                .await
        else {
            info!("received Ctrl-C during rebuild; shutting down");
            return Ok(());
        };
        (solver, controller) = built;
    }
    if let Some(runtime) = sinks.apex {
        runtime.shutdown();
    }
    solver.shutdown();
    Ok(())
}

/// Load worker pools like `fynd serve`: the default path falls back to the built-in default
/// pools when absent; custom paths that don't exist fail fast.
fn load_pools_config(cfg: &MonitorArgs) -> anyhow::Result<fynd_rpc::config::WorkerPoolsConfig> {
    let default_path = std::path::Path::new("worker_pools.toml");
    if cfg.worker_pools_config.as_path() == default_path && !default_path.exists() {
        info!("worker_pools.toml not found; using Fynd's built-in default pools");
        return Ok(fynd_rpc::config::WorkerPoolsConfig::builtin_default());
    }
    fynd_rpc::config::WorkerPoolsConfig::load_from_file(&cfg.worker_pools_config).map_err(|e| {
        anyhow::anyhow!(
            "failed to load worker pools config {}: {e}",
            cfg.worker_pools_config.display()
        )
    })
}

/// The session's output sinks — everything a resolved block lands in besides Prometheus: the
/// comparisons JSONL, the batch-capture JSONL, and the APEX batch stage.
struct Sinks {
    comparisons: Option<super::jsonl::RotatingWriter>,
    captures: Option<super::jsonl::RotatingWriter>,
    apex: Option<super::apex_live::ApexRuntime>,
}

impl Sinks {
    fn from_args(cfg: &MonitorArgs) -> anyhow::Result<Self> {
        let comparisons = match cfg.comparisons_dir.as_ref() {
            Some(dir) => {
                let writer = super::jsonl::RotatingWriter::open(dir, "comparisons")?;
                info!(path = %writer.current_path().display(), "appending comparisons to JSONL");
                Some(writer)
            }
            None => None,
        };
        let captures = match cfg.capture_dir.as_ref() {
            Some(dir) => {
                let writer = super::jsonl::RotatingWriter::open(dir, "batches")?;
                info!(path = %writer.current_path().display(), "appending batch snapshots to JSONL");
                Some(writer)
            }
            None => None,
        };
        let apex = super::apex_live::ApexRuntime::from_args(cfg)?;
        Ok(Self { comparisons, captures, apex })
    }
}

/// Drive one solver session: step blocks and re-solve each block's settled trades until the run
/// completes or the feed dies.
async fn run_session<P: Provider>(
    cfg: &MonitorArgs,
    pacing: &Pacing,
    adapter: &StepAdapter<'_>,
    decoder: &mut Decoder<P>,
    sinks: &mut Sinks,
    totals: &mut Totals,
) -> SessionEnd {
    // Establish a baseline applied state (N-1) before the first comparison.
    if adapter.current_block().await.is_none() {
        info!("waiting for solver to apply its first block…");
        if let Err(e) = adapter.advance().await {
            return SessionEnd::Unhealthy(e.to_string());
        }
    }

    loop {
        if adapter
            .controller
            .peek_next_block()
            .await
            .is_none()
        {
            return SessionEnd::Unhealthy("block stream ended".to_string());
        }
        let Some(top_block) = adapter.current_block().await else {
            warn!("no applied block yet; advancing");
            if let Err(e) = adapter.advance().await {
                return SessionEnd::Unhealthy(e.to_string());
            }
            continue;
        };
        let target = top_block + 1;

        // A healthy monitor keeps pace with the chain. Skip the check on a transient RPC error —
        // rebuilding is expensive (minutes of token loading), so only a confirmed lag triggers it.
        if let Ok(head) = decoder
            .provider()
            .get_block_number()
            .await
        {
            telemetry::record_head_lag_blocks(head.saturating_sub(target));
            if head.saturating_sub(target) > pacing.max_lag_blocks {
                return SessionEnd::Unhealthy(format!(
                    "monitor is {} blocks behind head {head}; presuming an unhealthy session",
                    head - target
                ));
            }
        }

        let trades = match decode_block_when_available(decoder, target, pacing.rpc_lag_budget).await
        {
            Ok(trades) => trades,
            Err(e) => {
                totals.skipped_blocks += 1;
                telemetry::record_skipped_block();
                warn!(
                    block = target,
                    skipped_total = totals.skipped_blocks,
                    "decode failed, skipping block: {e}"
                );
                if let Err(e) = adapter.advance().await {
                    return SessionEnd::Unhealthy(e.to_string());
                }
                continue;
            }
        };

        if let Err(reason) = resolve_block(cfg, adapter, decoder, sinks, &trades, target).await {
            return SessionEnd::Unhealthy(reason);
        }
        totals.processed += 1;
        if cfg
            .max_blocks
            .is_some_and(|max| totals.processed >= max)
        {
            info!(processed = totals.processed, "reached --max-blocks");
            return SessionEnd::Complete;
        }
    }
}

/// Re-solve one decoded block through the three phases — tops at N-1, the advance, backs at N —
/// and land the results in every sink. The APEX brackets dispatch at their seams: top on the
/// still-live N-1 state after the tops are known, bottom once the solver holds N. `Err` carries
/// the unhealthy-session reason when the advance fails.
async fn resolve_block<P: Provider>(
    cfg: &MonitorArgs,
    adapter: &StepAdapter<'_>,
    decoder: &Decoder<P>,
    sinks: &mut Sinks,
    trades: &[DecodedTrade],
    target: u64,
) -> Result<(), String> {
    let start = Instant::now();
    // Snapshot token prices at top-of-block (N-1) for the headline metric and the top-of-block
    // USD valuation.
    let prices_top = snapshot_prices(adapter.solver, decoder.registry()).await;
    let tops = solve_tops(adapter, trades).await;
    // Pre-advance seam: the N-1 state is still live and the block's tops are known. The APEX
    // batch stage clones its filtered pool subset here and solves off the critical path.
    let apex_eligible = sinks.apex.is_some() && super::apex_live::should_dispatch(trades);
    if apex_eligible {
        super::apex_live::dispatch_bracket(
            sinks.apex.as_ref(),
            adapter.solver,
            trades,
            target,
            super::apex_live::Bracket::Top,
        )
        .await;
    }
    if let Err(e) = adapter.advance().await {
        return Err(e.to_string());
    }
    let ranges = solve_backs(adapter, trades, tops, &prices_top).await;
    // Bottom bracket: the solver now holds N, the biased-bottom state (mirrors fynd's back).
    if apex_eligible {
        super::apex_live::dispatch_bracket(
            sinks.apex.as_ref(),
            adapter.solver,
            trades,
            target,
            super::apex_live::Bracket::Bottom,
        )
        .await;
    }
    if let Some(runtime) = sinks.apex.as_mut() {
        runtime.drain();
    }
    // The solver now holds back-of-block (N); snapshot again so the back-of-block improvement
    // is valued against the state it was solved at.
    let prices_back = snapshot_prices(adapter.solver, decoder.registry()).await;
    // The back-of-block solve should land on `target`. On a reorg/gap/resync the stream can
    // apply a different block, silently pairing the back state with another block's trades.
    // The top-of-block (N-1) headline is unaffected; warn so the mispaired back state is
    // visible.
    let applied = adapter.current_block().await;
    if applied != Some(target) {
        warn!(
            target,
            applied = ?applied,
            "back-of-block state is not the target block; back comparison may be off"
        );
    }
    for range in &ranges {
        telemetry::record_range(
            range,
            &cfg.chain.name,
            &prices_top,
            &prices_back,
            decoder.registry(),
        );
    }
    if let Some(rotating) = sinks.comparisons.as_mut() {
        super::jsonl::write_comparisons(rotating.writer(), &ranges, &prices_top, &prices_back);
    }
    // Captured at top-of-block prices: the batch is replayed against state N-1, so its
    // starting price view must be the one Fynd's baseline was solved under, not the
    // post-block one the back state is valued at.
    if let Some(rotating) = sinks.captures.as_mut() {
        let snapshot = crate::capture::build_snapshot(target, trades, &ranges, &prices_top);
        crate::capture::write_snapshot(rotating.writer(), &snapshot);
    }
    let elapsed_s = start.elapsed().as_secs_f64();
    telemetry::record_block_seconds(elapsed_s);

    info!(block = target, trades = ranges.len(), elapsed_s, "re-solved block (top/back)");
    Ok(())
}

/// Snapshot the solver's current token prices as `Prices` (token native-units per wei of
/// the gas token), anchored by `registry`'s USD anchor tokens. Empty until the first
/// derived-data computation completes; tokens with an unconvertible price are skipped.
async fn snapshot_prices(solver: &Solver, registry: &Registry) -> Prices {
    let mut prices = Prices::new(registry);
    let derived = solver.derived_data();
    let guard = derived.read().await;
    let Some(token_prices) = guard.token_prices() else {
        return prices;
    };
    for (token, price) in token_prices {
        let (Ok(numerator), Ok(denominator)) = (
            price
                .numerator
                .to_string()
                .parse::<f64>(),
            price
                .denominator
                .to_string()
                .parse::<f64>(),
        ) else {
            continue;
        };
        if denominator <= 0.0 {
            continue;
        }
        if let Some(address) = core_to_alloy(token) {
            prices.insert(address, numerator / denominator);
        }
    }
    prices
}

/// Convert a tycho-core 20-byte address to an alloy `Address`.
fn core_to_alloy(address: &CoreAddress) -> Option<Address> {
    let bytes: &[u8] = address.as_ref();
    (bytes.len() == 20).then(|| Address::from_slice(bytes))
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
    fn test_pacing_scales_both_budgets_with_block_time() {
        // The RPC-lag budget is a block count, so it must shrink on a fast chain: a fixed
        // seconds budget spends several Base blocks waiting for a block that already landed.
        let base = Pacing::for_chain(Chain::Base, None);
        assert_eq!(base.max_lag_blocks, 600);
        assert_eq!(base.rpc_lag_budget, Duration::from_secs(6));

        let ethereum = Pacing::for_chain(Chain::Ethereum, None);
        assert_eq!(ethereum.max_lag_blocks, 100);
        assert_eq!(ethereum.rpc_lag_budget, Duration::from_secs(36));

        // An explicit --max-lag-blocks overrides only the lag threshold.
        let overridden = Pacing::for_chain(Chain::Base, Some(7));
        assert_eq!(overridden.max_lag_blocks, 7);
        assert_eq!(overridden.rpc_lag_budget, Duration::from_secs(6));
    }

    /// A mocked provider whose `eth_blockNumber` answers come from `heads`, in order.
    fn decoder_with_heads(heads: &[u64]) -> Decoder<impl Provider> {
        use alloy::providers::{mock::Asserter, ProviderBuilder};

        let asserter = Asserter::new();
        for head in heads {
            asserter.push_success(&format!("0x{head:x}"));
        }
        Decoder::new(
            ProviderBuilder::default().connect_mocked_client(asserter),
            Registry::ethereum(),
        )
    }

    #[tokio::test]
    async fn test_decode_waits_only_while_the_rpc_lags() {
        // Head already covers the target: no wait, straight to decoding (which then fails on the
        // receipts call the mock has no answer for — proof it got that far).
        let mut ready = decoder_with_heads(&[100]);
        let started = Instant::now();
        assert!(decode_block_when_available(&mut ready, 100, Duration::from_secs(6))
            .await
            .is_err());
        assert!(started.elapsed() < DECODE_RPC_LAG_POLL, "waited despite an indexed block");
    }

    #[tokio::test]
    async fn test_decode_gives_up_after_the_lag_budget() {
        // Head never reaches the target. A zero budget proves the wait is bounded by the budget
        // alone — the old fixed backoff slept before ever re-checking, so it could not express
        // "don't wait".
        let mut lagging = decoder_with_heads(&[99]);
        let started = Instant::now();
        assert!(decode_block_when_available(&mut lagging, 100, Duration::ZERO)
            .await
            .is_err());
        assert!(started.elapsed() < DECODE_RPC_LAG_POLL, "slept despite a spent budget");
    }

    /// End-to-end smoke test of the live two-state monitor against a real solver.
    ///
    /// `#[ignore]`d so it never runs in CI (no Tycho/RPC). Run with:
    /// `TYCHO_URL=<ws> RPC_URL=<https> cargo test -p hindsight --bin hindsight \
    ///   resolve::monitor -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires live TYCHO_URL + RPC_URL"]
    async fn test_monitor_one_block_smoke() {
        let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL");
        let tycho_url = std::env::var("TYCHO_URL").expect("set TYCHO_URL");
        let api_key = std::env::var("TYCHO_API_KEY").ok();
        run(MonitorArgs {
            chain: crate::ChainArgs { name: "ethereum".to_string(), rpc_url, registry: None },
            tycho_url,
            protocols: vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
            // High TVL floor → fewer pools → faster load for a smoke test.
            min_tvl: 10_000.0,
            tycho_api_key: api_key,
            worker_pools_config: std::path::PathBuf::from("worker_pools.toml"),
            timeout_ms: fynd_rpc::config::defaults::WORKER_ROUTER_TIMEOUT_MS,
            metrics_port: None,
            max_blocks: Some(1),
            max_lag_blocks: Some(100),
            comparisons_dir: None,
            capture_dir: None,
            apex_dir: None,
            apex_workers: 2,
            apex_queue_capacity: 8,
            apex_budget_ms: 1_000,
            apex_single_budget_ms: 250,
            apex_max_pools: 400,
        })
        .await
        .expect("monitor should process one block without error");
    }
}
