//! Live two-state monitor: drive an in-process `fynd-core` solver one block at a time, re-solving
//! each block's settled trades at top-of-block (N-1) and back-of-block (N).
//!
//! The block barrier is deterministic: after releasing a block via
//! `BlockStepController::trigger_next_block`, we wait until the solver's `MarketData` reports the
//! next applied block before re-solving back-of-block. The pure orchestration is unit-tested in the
//! parent module via a mock `SteppingSolver`; this live driver is exercised by the gated
//! integration test in `tests/` (requires `TYCHO_URL` + `RPC_URL`).

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use async_trait::async_trait;
use fynd_core::{
    types::{
        parse_chain, EncodingOptions, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest,
        QuoteStatus,
    },
    BlockStepController, FyndBuilder, LiquidityScope, Solver,
};
use num_bigint::BigUint;
use tracing::{debug, info, warn};
use tycho_simulation::tycho_common::models::{Address as CoreAddress, Chain};

use crate::{
    decoder::{DecodedTrade, Decoder, Registry},
    propamm, provider_from,
    resolve::{resolve_block_range, Outcome, SolvedAmount, SteppingSolver},
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

    /// Per-quote timeout in milliseconds
    #[arg(long, default_value_t = 10_000)]
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

    /// Mirror a mock `PropAMM` pool onto this token pair, as two comma-separated addresses
    /// (`--propamm-pair 0xWETH,0xUSDC`). The mock carries the best real pool's live curve at the
    /// price set by `--propamm-price-pct` and charges no fee. It is hidden from the public worker
    /// pools and visible to a parallel exclusive-access twin of each configured pool, so each
    /// re-solved order reports whether the `PropAMM` route won and how much fee it could have
    /// charged on top and still won. Off when omitted, and never usable against a real chain — the
    /// mock has no pool behind it
    #[arg(long, env = "PROPAMM_PAIR", value_delimiter = ',')]
    pub propamm_pair: Option<Vec<String>>,

    /// The mock pool's fee-free price, as a percentage of the best real pool's price for the pair.
    /// `100` positions it exactly at the best price we can see (the control case — it cannot then
    /// strictly beat the public market); `100.05` positions it 5 bps better. The fee it could
    /// charge on top is measured, not set here
    #[arg(long, env = "PROPAMM_PRICE_PCT", default_value_t = 100.0)]
    pub propamm_price_pct: f64,

    /// Trade size, in whole units of the pair's first token, used each block to pick which real
    /// pool to mirror. Set it near the sizes being re-solved so the mirror tracks the pool that
    /// actually prices those trades best
    #[arg(long, env = "PROPAMM_PROBE_UNITS", default_value_t = 1.0)]
    pub propamm_probe_units: f64,

    /// Append one JSON line per re-solved order that the mock `PropAMM` won, with the committed
    /// output, the fee headroom in bps, and both valued in USD
    #[arg(long, env = "PROPAMM_JSONL")]
    pub propamm_jsonl: Option<std::path::PathBuf>,
}

/// The mock-`PropAMM` scaffold attached to one monitor run.
///
/// `stats` is created once per run so the totals span solver rebuilds; `injector` is rebuilt with
/// each solver, because it publishes on that solver's market-event channel.
struct PropAmmHarness {
    injector: tokio::sync::Mutex<propamm::Injector>,
    stats: Arc<propamm::report::Stats>,
    jsonl: Option<std::path::PathBuf>,
}

/// Drives the in-process solver, stepping the chain one block per `SteppingSolver::advance`.
struct StepAdapter<'a> {
    solver: &'a Solver,
    controller: &'a BlockStepController,
    timeout_ms: u64,
    /// Present when `--propamm-pair` is set; drives mock-pool injection and collects its outcomes.
    propamm: Option<&'a PropAmmHarness>,
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

    /// Releases the next block and waits until the solver applies it.
    ///
    /// An error means the feed died — either its stream ended (peek returns None once the gating
    /// task exits) or it jammed without ending (no block within `FEED_DEAD_TIMEOUT`). The caller
    /// rebuilds the solver on any error.
    async fn step_block(&self) -> anyhow::Result<()> {
        let before = self.current_block().await;
        self.controller
            .trigger_next_block()
            .map_err(|_| anyhow::anyhow!("tycho stream ended (trigger channel closed)"))?;

        // Deterministic barrier: wait until the solver applies a block strictly newer than
        // `before`.
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

    /// Re-mirrors the mock `PropAMM` pool onto the freshly applied block's state.
    ///
    /// A failure here leaves the mock holding the previous block's state, which skews its quotes
    /// but does not invalidate the block's public comparison — so it warns and continues rather
    /// than ending the session.
    async fn mirror_propamm_pool(&self) {
        let Some(harness) = self.propamm else {
            return;
        };
        let Some(block) = self.current_block().await else {
            return;
        };
        match harness
            .injector
            .lock()
            .await
            .inject(self.solver, block)
            .await
        {
            Ok(Some(injected)) => debug!(
                block,
                source = injected.source_component,
                source_price = injected.source_price,
                mock_price = injected.mock_price,
                derived_data_ready = injected.derived_data_ready,
                "mirrored the mock PropAMM pool"
            ),
            Ok(None) => warn!(
                block,
                "no source pool for the mirrored pair carries state yet; the mock PropAMM is \
                 inactive this block"
            ),
            Err(e) => warn!(block, "failed to mirror the mock PropAMM pool: {e}"),
        }
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
            amount.clone(),
            OrderSide::Sell,
            CoreAddress::from([0x11u8; 20]),
        );
        let request = QuoteRequest::new(
            vec![order],
            QuoteOptions::default()
                .with_timeout_ms(self.timeout_ms)
                .with_encoding_options(EncodingOptions::new(0.005)),
        );

        match self.solver.quote(request).await {
            Ok(quote) => {
                let Some(order_quote) = quote.orders().first() else {
                    return Outcome::Unsolvable("solver returned no order quote".to_string());
                };
                if let Some(harness) = self.propamm {
                    // Only successful quotes count, so the winrate's denominator is orders the
                    // PropAMM actually had a chance at.
                    if order_quote.status() == QuoteStatus::Success {
                        harness
                            .stats
                            .record(propamm::report::Observation::from_quote(
                                order_quote,
                                token_in,
                                token_out,
                                amount,
                            ));
                    }
                }
                order_quote_to_outcome(order_quote)
            }
            Err(e) => Outcome::Unsolvable(format!("solve error: {e}")),
        }
    }

    async fn advance(&self) -> anyhow::Result<()> {
        self.step_block().await?;
        self.mirror_propamm_pool().await;
        Ok(())
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
        quote_json,
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
    if cfg.propamm_pair.is_some() {
        // Twin every configured pool with an exclusive-access copy: same algorithm and hop limits,
        // so the two scopes differ only in whether they can see the mock pool. Anything less would
        // confound the PropAMM's advantage with an algorithm difference. The unscoped originals
        // stay public — `FyndBuilder` hands the exclusivity policy to every pool that does not
        // opt into `LiquidityScope::All`.
        builder = builder.exclusivity_policy(propamm::is_mock_component);
        for (name, pool) in pools_config.pools() {
            let twin = pool
                .clone()
                .with_liquidity_scope(LiquidityScope::All);
            let twin_name = format!("{name}__propamm");
            builder = builder
                .add_pool(&twin_name, &twin)
                .map_err(|e| anyhow::anyhow!("failed to add worker pool {twin_name}: {e}"))?;
        }
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
    let chain = parse_chain(&cfg.chain.name)
        .map_err(|e| anyhow::anyhow!("invalid --chain '{}': {e}", cfg.chain.name))?;

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

    let pools_config = load_pools_config(&cfg.worker_pools_config)?;

    let mut decoder = Decoder::new(
        provider_from(&cfg.chain.rpc_url)?,
        Registry::load(&cfg.chain.name, cfg.chain.registry.as_deref())?,
    );

    if let Some(port) = cfg.metrics_port {
        telemetry::install_exporter(port)?;
        info!(port, "serving Prometheus metrics at /metrics");
    }

    let mut comparisons = match cfg.comparisons_dir.as_ref() {
        Some(dir) => {
            let writer = super::jsonl::RotatingWriter::open(dir)?;
            info!(path = %writer.current_path().display(), "appending comparisons to JSONL");
            Some(writer)
        }
        None => None,
    };

    // Parsed before the first (multi-minute) solver build, so a typo in the pair fails immediately.
    let propamm_config = propamm_config(&cfg, chain)?;
    let propamm_stats = Arc::new(propamm::report::Stats::default());

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
        // Rebuilt with each solver: the injector publishes on that solver's market-event channel.
        // `propamm_stats` is shared, so the totals span rebuilds.
        let propamm = propamm_config
            .clone()
            .map(|config| PropAmmHarness {
                injector: tokio::sync::Mutex::new(propamm::Injector::new(&solver, config)),
                stats: Arc::clone(&propamm_stats),
                jsonl: cfg.propamm_jsonl.clone(),
            });
        let adapter = StepAdapter {
            solver: &solver,
            controller: &controller,
            timeout_ms: cfg.timeout_ms,
            propamm: propamm.as_ref(),
        };
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
                &mut comparisons,
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
    solver.shutdown();
    if let Some(config) = propamm_config.as_ref() {
        log_propamm_summary(config, &propamm_stats.totals());
    }
    Ok(())
}

/// Loads the worker pools like `fynd serve` does.
///
/// The default path falls back to the built-in default pools when absent; a custom path that does
/// not exist fails fast, because an operator who named a file meant that file.
fn load_pools_config(
    path: &std::path::Path,
) -> anyhow::Result<fynd_rpc::config::WorkerPoolsConfig> {
    let default_path = std::path::Path::new("worker_pools.toml");
    if path == default_path && !default_path.exists() {
        info!("worker_pools.toml not found; using Fynd's built-in default pools");
        return Ok(fynd_rpc::config::WorkerPoolsConfig::builtin_default());
    }
    fynd_rpc::config::WorkerPoolsConfig::load_from_file(path)
        .map_err(|e| anyhow::anyhow!("failed to load worker pools config {}: {e}", path.display()))
}

/// Builds the mock-`PropAMM` config from the CLI, or `None` when `--propamm-pair` is absent.
fn propamm_config(
    cfg: &MonitorArgs,
    chain: Chain,
) -> anyhow::Result<Option<propamm::MirrorConfig>> {
    let Some(pair) = cfg.propamm_pair.as_deref() else {
        return Ok(None);
    };
    let (token_a, token_b) = propamm::MirrorConfig::parse_pair(pair)?;
    let config = propamm::MirrorConfig {
        token_a,
        token_b,
        price_pct: cfg.propamm_price_pct,
        probe_units: cfg.propamm_probe_units,
        chain,
    };
    info!(
        token_a = %config.token_a,
        token_b = %config.token_b,
        price_pct = config.price_pct,
        probe_units = config.probe_units,
        "mock PropAMM enabled; its quotes are not executable"
    );
    Ok(Some(config))
}

/// Logs the whole run's mock-`PropAMM` result: the assumption that went in, and the winrate and fee
/// headroom that came out.
fn log_propamm_summary(config: &propamm::MirrorConfig, totals: &propamm::report::Totals) {
    info!(
        price_pct = config.price_pct,
        solved_orders = totals.solved,
        propamm_wins = totals.won,
        winrate_pct = format!("{:.1}", totals.winrate_pct()),
        captured_flow_usd = format!("{:.0}", totals.captured_flow_usd),
        fee_headroom_usd = format!("{:.2}", totals.headroom_usd),
        fee_headroom_bps = format!("{:.2}", totals.avg_fee_headroom_bps()),
        "mock PropAMM run summary"
    );
}

/// Values one block's mock-`PropAMM` observations in USD, folds them into the run totals, logs the
/// running picture, and appends the wins to JSONL.
///
/// Amounts are valued at top-of-block prices, matching the headline improvement metric. Both halves
/// are valued: the committed output answers "how much flow the pool captured", the headroom answers
/// "how much fee it could have charged on that flow", and their ratio is that fee in bps.
fn report_propamm_block(
    harness: &PropAmmHarness,
    block: u64,
    prices: &Prices,
    observations: &[propamm::report::Observation],
) {
    let mut headroom_usd = 0.0;
    let mut captured_flow_usd = 0.0;
    let mut lines = Vec::new();

    for observed in observations.iter().filter(|o| o.won) {
        let value_usd = |amount: Option<&BigUint>| {
            amount
                .and_then(biguint_to_u256_opt)
                .and_then(|amount| prices.value_usd(observed.token_out, amount))
        };
        let headroom = value_usd(observed.fee_headroom.as_ref());
        let committed = value_usd(observed.committed_amount_out.as_ref());
        headroom_usd += headroom.unwrap_or(0.0);
        captured_flow_usd += committed.unwrap_or(0.0);

        if harness.jsonl.is_some() {
            lines.push(serde_json::json!({
                "block": block,
                "token_in": observed.token_in.to_string(),
                "token_out": observed.token_out.to_string(),
                "amount_in": observed.amount_in.to_string(),
                "committed_amount_out": observed
                    .committed_amount_out
                    .as_ref()
                    .map(std::string::ToString::to_string),
                "fee_headroom": observed
                    .fee_headroom
                    .as_ref()
                    .map(std::string::ToString::to_string),
                "fee_headroom_bps": observed.fee_headroom_bps(),
                "committed_usd": committed,
                "fee_headroom_usd": headroom,
            }));
        }
    }

    let totals = harness
        .stats
        .accumulate(observations, headroom_usd, captured_flow_usd);
    let block_wins = observations
        .iter()
        .filter(|o| o.won)
        .count();
    info!(
        block,
        block_orders = observations.len(),
        block_wins,
        block_fee_headroom_usd = format!("{headroom_usd:.2}"),
        run_winrate_pct = format!("{:.1}", totals.winrate_pct()),
        run_fee_headroom_usd = format!("{:.2}", totals.headroom_usd),
        run_fee_headroom_bps = format!("{:.2}", totals.avg_fee_headroom_bps()),
        "mock PropAMM block result"
    );

    if let Some(path) = harness.jsonl.as_ref() {
        if let Err(e) = append_jsonl(path, &lines) {
            warn!(path = %path.display(), "failed to append PropAMM JSONL: {e}");
        }
    }
}

/// Appends one JSON object per line, creating the file if absent.
fn append_jsonl(path: &std::path::Path, lines: &[serde_json::Value]) -> std::io::Result<()> {
    use std::io::Write as _;

    if lines.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// Converts a `BigUint` to `U256`, or `None` when it does not fit — the amount is then left
/// unvalued rather than silently reported as zero.
fn biguint_to_u256_opt(value: &BigUint) -> Option<U256> {
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        return None;
    }
    let mut buf = [0u8; 32];
    buf[32 - bytes.len()..].copy_from_slice(&bytes);
    Some(U256::from_be_bytes(buf))
}

/// Drive one solver session: step blocks and re-solve each block's settled trades until the run
/// completes or the feed dies.
async fn run_session<P: Provider>(
    cfg: &MonitorArgs,
    pacing: &Pacing,
    adapter: &StepAdapter<'_>,
    decoder: &mut Decoder<P>,
    comparisons: &mut Option<super::jsonl::RotatingWriter>,
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

        let start = Instant::now();
        // Snapshot token prices at top-of-block (N-1) for the headline metric and the top-of-block
        // USD valuation.
        let prices_top = snapshot_prices(adapter.solver, decoder.registry()).await;
        let ranges = match resolve_block_range(adapter, &trades, &prices_top).await {
            Ok(ranges) => ranges,
            Err(e) => return SessionEnd::Unhealthy(e.to_string()),
        };
        // resolve_block_range advanced the solver to back-of-block (N); snapshot again so the
        // back-of-block improvement is valued against the state it was solved at.
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
        if let Some(rotating) = comparisons.as_mut() {
            super::jsonl::write_comparisons(rotating.writer(), &ranges, &prices_top, &prices_back);
        }
        // Drained unconditionally when the harness is on, so a block whose trades all failed to
        // solve still clears the sink rather than carrying observations into the next block.
        if let Some(harness) = adapter.propamm {
            let observations = harness.stats.drain();
            report_propamm_block(harness, target, &prices_top, &observations);
        }
        let elapsed_s = start.elapsed().as_secs_f64();
        telemetry::record_block_seconds(elapsed_s);

        info!(block = target, trades = ranges.len(), elapsed_s, "re-solved block (top/back)");

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
            timeout_ms: 10_000,
            metrics_port: None,
            max_blocks: Some(1),
            max_lag_blocks: Some(100),
            comparisons_dir: None,
            propamm_pair: None,
            propamm_price_pct: 100.05,
            propamm_probe_units: 1.0,
            propamm_jsonl: None,
        })
        .await
        .expect("monitor should process one block without error");
    }
}
