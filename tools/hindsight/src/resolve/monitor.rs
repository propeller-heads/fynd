//! Live two-state monitor: drive an in-process `fynd-core` solver one block at a time, re-solving
//! each block's settled trades at top-of-block (N-1) and back-of-block (N).
//!
//! The block barrier is deterministic: after releasing a block via
//! [`BlockStepController::trigger_next_block`], we wait until the solver's `MarketData` reports the
//! next applied block before re-solving back-of-block. The pure orchestration is unit-tested in the
//! parent module via a mock [`SteppingSolver`]; this live driver is exercised by the gated
//! integration test in `tests/` (requires `TYCHO_URL` + `RPC_URL`).

use std::time::{Duration, Instant};

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
    BlockStepController, FyndBuilder, Solver,
};
use num_bigint::BigUint;
use tracing::{info, warn};
use tycho_simulation::tycho_common::models::{Address as CoreAddress, Chain};

use crate::{
    decoder::{DecodedTrade, Decoder, Registry},
    provider_from,
    resolve::{resolve_block_range, Outcome, SolvedAmount, SteppingSolver},
    telemetry, usd,
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

/// The HTTP RPC used to decode receipts can trail the Tycho stream by a few seconds, so `target`
/// (which tracks the stream's tip) may not be indexed yet on the first look. Wait for the RPC head
/// to reach it, retrying a bounded number of times before treating it as a genuine failure.
const DECODE_RPC_LAG_RETRIES: usize = 5;
const DECODE_RPC_LAG_BACKOFF: Duration = Duration::from_millis(1500);

/// Falling this many blocks (~20 min at 12s) behind chain head means the session is crippled
/// without being dead — seen live: a worker lost its derived-data channel, every solve crawled,
/// the feed-dead watchdog never fired (blocks still trickled through), and the monitor slid
/// hours behind while warn-spamming. The remedy is the same as a dead feed: tear down and
/// rebuild, which resubscribes at head and keeps the data gap bounded.
const MAX_LAG_BLOCKS: u64 = 100;

/// Inputs for the live monitor — the `monitor` subcommand's CLI arguments, used directly as its
/// configuration.
#[derive(clap::Args)]
pub(crate) struct MonitorArgs {
    #[command(flatten)]
    pub rpc: crate::RpcArgs,

    /// Tycho WebSocket URL feeding the in-process solver
    #[arg(long, env = "TYCHO_URL")]
    pub tycho_url: String,

    /// Chain to monitor (the decoder is Ethereum-only for now)
    #[arg(long, default_value = "ethereum")]
    pub chain: String,

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

    /// Write one JSON line per re-solved trade (every comparison — wins, losses, and unsolvable
    /// coverage gaps) into this directory as `comparisons-YYYY-MM-DD.jsonl`, rotated at each UTC
    /// day boundary so an external sync job can ship closed daily files. Each record carries
    /// both block states with verdict, bps, USD delta, and a slim route/calldata or unsolvable
    /// reason; filter downstream for the improvement or coverage view
    #[arg(long)]
    pub comparisons_dir: Option<std::path::PathBuf>,
}

/// Drives the in-process solver, stepping the chain one block per [`SteppingSolver::advance`].
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

        match self.solver.quote(request).await {
            Ok(quote) => quote
                .orders()
                .first().map_or_else(|| {
                    Outcome::Unsolvable("solver returned no order quote".to_string())
                }, order_quote_to_outcome),
            Err(e) => Outcome::Unsolvable(format!("solve error: {e}")),
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
/// Tycho stream that drives `block`, so poll the RPC head until it reaches `block` (bounded retries
/// with backoff) — that distinguishes a transient race from a real failure. A block still
/// undecodable once the RPC has indexed it is a genuine error and surfaces to the caller.
async fn decode_block_when_available<P: Provider>(
    decoder: &mut Decoder<P>,
    block: u64,
) -> anyhow::Result<Vec<DecodedTrade>> {
    for attempt in 0..DECODE_RPC_LAG_RETRIES {
        let head = match decoder.provider().get_block_number().await {
            Ok(h) => h,
            Err(e) => {
                warn!(block, attempt, "failed to fetch RPC block number: {e}");
                0
            }
        };
        if head >= block {
            break;
        }
        warn!(block, head, attempt, "RPC lags the tycho stream; waiting for it to index the block");
        tokio::time::sleep(DECODE_RPC_LAG_BACKOFF).await;
    }
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
        chain = cfg.chain,
        protocols = protocols.len(),
        "building in-process solver (loading tokens may take minutes)…"
    );
    let mut builder =
        FyndBuilder::new(chain, &cfg.tycho_url, &cfg.rpc.rpc_url, protocols.to_vec(), cfg.min_tvl);
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

/// Build the in-process stepped solver and re-solve each block's settled trades as a top/back
/// range. When the tycho feed dies (its stream ends, or no block arrives within
/// [`FEED_DEAD_TIMEOUT`]), the solver is torn down and rebuilt in place — fresh subscriptions,
/// same decoder cache and comparisons file — so a long unattended run survives feed failures.
pub(crate) async fn run(cfg: MonitorArgs) -> anyhow::Result<()> {
    let chain = parse_chain(&cfg.chain)
        .map_err(|e| anyhow::anyhow!("invalid --chain '{}': {e}", cfg.chain))?;

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

    // Load worker pools like `fynd serve`: the default path falls back to the built-in default
    // pools when absent; custom paths that don't exist fail fast.
    let default_path = std::path::Path::new("worker_pools.toml");
    let pools_config =
        if cfg.worker_pools_config.as_path() == default_path && !default_path.exists() {
            info!("worker_pools.toml not found; using Fynd's built-in default pools");
            fynd_rpc::config::WorkerPoolsConfig::builtin_default()
        } else {
            fynd_rpc::config::WorkerPoolsConfig::load_from_file(&cfg.worker_pools_config).map_err(
                |e| {
                    anyhow::anyhow!(
                        "failed to load worker pools config {}: {e}",
                        cfg.worker_pools_config.display()
                    )
                },
            )?
        };

    let mut decoder = Decoder::new(
        provider_from(&cfg.rpc.rpc_url)?,
        Registry::load(&cfg.chain, cfg.rpc.registry.as_deref())?,
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

    let mut totals = Totals::default();
    // The first build fails fast — an error here is a configuration problem. Rebuilds after a
    // feed death retry forever, since the config is known good and failures are transient.
    let (mut solver, mut controller) = build_solver(&cfg, chain, &protocols, &pools_config).await?;
    loop {
        let adapter =
            StepAdapter { solver: &solver, controller: &controller, timeout_ms: cfg.timeout_ms };
        match run_session(&cfg, &adapter, &mut decoder, &mut comparisons, &mut totals).await {
            SessionEnd::Complete => return Ok(()),
            SessionEnd::Unhealthy(reason) => {
                warn!(reason, "session unhealthy; rebuilding the solver to resubscribe");
                telemetry::record_feed_rebuild();
            }
        }
        solver.shutdown();
        (solver, controller) = loop {
            tokio::time::sleep(REBUILD_BACKOFF).await;
            match build_solver(&cfg, chain, &protocols, &pools_config).await {
                Ok(built) => break built,
                Err(e) => warn!(error = %e, "solver rebuild failed; retrying"),
            }
        };
    }
}

/// Drive one solver session: step blocks and re-solve each block's settled trades until the run
/// completes or the feed dies.
async fn run_session<P: Provider>(
    cfg: &MonitorArgs,
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
            if head.saturating_sub(target) > MAX_LAG_BLOCKS {
                return SessionEnd::Unhealthy(format!(
                    "monitor is {} blocks behind head {head}; presuming a crippled session",
                    head - target
                ));
            }
        }

        let trades = match decode_block_when_available(decoder, target).await {
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
        let prices_top = snapshot_prices(adapter.solver).await;
        let ranges = match resolve_block_range(adapter, &trades, &prices_top).await {
            Ok(ranges) => ranges,
            Err(e) => return SessionEnd::Unhealthy(e.to_string()),
        };
        // resolve_block_range advanced the solver to back-of-block (N); snapshot again so the
        // back-of-block improvement is valued against the state it was solved at.
        let prices_back = snapshot_prices(adapter.solver).await;
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
                &cfg.chain,
                &prices_top,
                &prices_back,
                decoder.registry(),
            );
        }
        if let Some(rotating) = comparisons.as_mut() {
            super::jsonl::write_comparisons(rotating.writer(), &ranges, &prices_top, &prices_back);
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

/// Snapshot the solver's current token prices as a [`usd::PriceMap`] (token native-units per
/// ETH-wei). Empty until the first derived-data computation completes; tokens with an
/// unconvertible price are skipped.
async fn snapshot_prices(solver: &Solver) -> usd::PriceMap {
    let derived = solver.derived_data();
    let guard = derived.read().await;
    let Some(token_prices) = guard.token_prices() else {
        return usd::PriceMap::new();
    };
    let mut prices = usd::PriceMap::with_capacity(token_prices.len());
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

/// Convert a tycho-core 20-byte address to an alloy [`Address`].
fn core_to_alloy(address: &CoreAddress) -> Option<Address> {
    let bytes: &[u8] = address.as_ref();
    (bytes.len() == 20).then(|| Address::from_slice(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end smoke test of the live two-state monitor against a real solver.
    ///
    /// `#[ignore]`d so it never runs in CI (no Tycho/RPC). Run with:
    /// `TYCHO_URL=<ws> RPC_URL=<https> cargo test -p hindsight --bin hindsight \
    ///   resolve::monitor -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires live TYCHO_URL + RPC_URL"]
    async fn monitor_one_block_smoke() {
        let (Ok(rpc_url), Ok(tycho_url)) = (std::env::var("RPC_URL"), std::env::var("TYCHO_URL"))
        else {
            eprintln!("skipping: set RPC_URL and TYCHO_URL");
            return;
        };

        let api_key = std::env::var("TYCHO_API_KEY").ok();
        run(MonitorArgs {
            rpc: crate::RpcArgs { rpc_url, registry: None },
            tycho_url,
            chain: "ethereum".to_string(),
            protocols: vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
            // High TVL floor → fewer pools → faster load for a smoke test.
            min_tvl: 10_000.0,
            tycho_api_key: api_key,
            worker_pools_config: std::path::PathBuf::from("worker_pools.toml"),
            timeout_ms: 10_000,
            metrics_port: None,
            max_blocks: Some(1),
            comparisons_dir: None,
        })
        .await
        .expect("monitor should process one block without error");
    }
}
