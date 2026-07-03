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
use tycho_simulation::tycho_common::models::Address as CoreAddress;

use crate::{
    decoder::{DecodedTrade, Decoder, Registry},
    provider_from,
    resolve::{resolve_block_range, Outcome, SolvedAmount, SteppingSolver, Verdict},
    telemetry, usd,
};

/// How long to wait for the solver to apply the next block after releasing it. Generous because the
/// Tycho stream periodically goes silent for minutes while it reconnects/resyncs the large
/// `all_onchain` synchronizer set; the stream recovers on its own, so the monitor should wait it
/// out rather than exit (which would reset all metrics). Only a truly dead feed should ever hit
/// this.
const BLOCK_SETTLE_TIMEOUT: Duration = Duration::from_secs(1800);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The HTTP RPC used to decode receipts can trail the Tycho stream by a few seconds, so `target`
/// (which tracks the stream's tip) may not be indexed yet on the first look. Wait for the RPC head
/// to reach it, retrying a bounded number of times before treating it as a genuine failure.
const DECODE_RPC_LAG_RETRIES: usize = 5;
const DECODE_RPC_LAG_BACKOFF: Duration = Duration::from_millis(1500);

/// Inputs for the live monitor.
pub(crate) struct MonitorConfig<'a> {
    pub rpc_url: &'a str,
    pub tycho_url: &'a str,
    pub chain: &'a str,
    pub protocols: Vec<String>,
    pub min_tvl: f64,
    pub tycho_api_key: Option<&'a str>,
    /// Worker-pools TOML config path; the default path falls back to Fynd's built-in default
    /// pools when absent, like `fynd serve`. Custom paths that don't exist fail fast.
    pub worker_pools_config: &'a std::path::Path,
    pub timeout_ms: u64,
    pub metrics_port: Option<u16>,
    /// Stop after this many blocks (`None` runs until interrupted).
    pub max_blocks: Option<u64>,
    /// Append one JSON line per re-solved trade (every comparison — wins, losses, and unsolvable
    /// coverage gaps), each carrying both block states with verdict, net bps, USD delta, and a
    /// slim route/calldata or unsolvable reason. Filter downstream for the improvement or
    /// coverage view. Disabled when `None`.
    pub comparisons_jsonl: Option<&'a str>,
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
            .map(|b| b.number())
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
                .first()
                .map(order_quote_to_outcome)
                .unwrap_or_else(|| {
                    Outcome::Unsolvable("solver returned no order quote".to_string())
                }),
            Err(e) => Outcome::Unsolvable(format!("solve error: {e}")),
        }
    }

    async fn advance(&self) -> anyhow::Result<()> {
        let before = self.current_block().await;
        self.controller
            .trigger_next_block()
            .map_err(|_| anyhow::anyhow!("block stream ended"))?;

        // Deterministic barrier: wait until the solver applies a block strictly newer than
        // `before`.
        let deadline = Instant::now() + BLOCK_SETTLE_TIMEOUT;
        loop {
            if let Some(now) = self.current_block().await {
                if before.is_none_or(|b| now > b) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for solver to apply the next block");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
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
    value
        .to_string()
        .parse()
        .unwrap_or(U256::ZERO)
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
        let head = decoder
            .provider()
            .get_block_number()
            .await
            .unwrap_or(0);
        if head >= block {
            break;
        }
        warn!(block, head, attempt, "RPC lags the tycho stream; waiting for it to index the block");
        tokio::time::sleep(DECODE_RPC_LAG_BACKOFF).await;
    }
    decoder.decode_block(block).await
}

/// Build the in-process stepped solver and re-solve each block's settled trades as a top/back
/// range.
pub(crate) async fn run(cfg: MonitorConfig<'_>) -> anyhow::Result<()> {
    let chain = parse_chain(cfg.chain)
        .map_err(|e| anyhow::anyhow!("invalid --chain '{}': {e}", cfg.chain))?;

    // Expand protocol tokens (e.g. `native_onchain`/`all_onchain`) against Tycho, like serve/scale.
    let protocols = fynd_rpc::protocols::resolve_protocols(
        cfg.tycho_url,
        cfg.tycho_api_key,
        true,
        chain,
        &cfg.protocols,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to resolve protocols: {e}"))?;
    info!(
        chain = cfg.chain,
        protocols = protocols.len(),
        "building in-process solver (loading tokens may take minutes)…"
    );
    let mut builder = FyndBuilder::new(chain, cfg.tycho_url, cfg.rpc_url, protocols, cfg.min_tvl);
    if let Some(key) = cfg.tycho_api_key {
        builder = builder.tycho_api_key(key);
    }
    // Load worker pools like `fynd serve`: the default path falls back to the built-in default
    // pools when absent; custom paths that don't exist fail fast.
    let default_path = std::path::Path::new("worker_pools.toml");
    let pools_config = if cfg.worker_pools_config == default_path && !default_path.exists() {
        info!("worker_pools.toml not found; using Fynd's built-in default pools");
        fynd_rpc::config::WorkerPoolsConfig::builtin_default()
    } else {
        fynd_rpc::config::WorkerPoolsConfig::load_from_file(cfg.worker_pools_config).map_err(
            |e| {
                anyhow::anyhow!(
                    "failed to load worker pools config {}: {e}",
                    cfg.worker_pools_config.display()
                )
            },
        )?
    };
    for (name, pool) in pools_config.pools() {
        builder = builder
            .add_pool(name, pool)
            .map_err(|e| anyhow::anyhow!("failed to add worker pool {name}: {e}"))?;
    }
    let (solver, controller) = builder
        .build_with_step_controller()
        .await
        .map_err(|e| anyhow::anyhow!("failed to build solver: {e}"))?;

    let mut decoder = Decoder::new(provider_from(cfg.rpc_url)?, Registry::for_chain(cfg.chain)?);

    if let Some(port) = cfg.metrics_port {
        telemetry::install_exporter(port)?;
        info!(port, "serving Prometheus metrics at /metrics");
    }

    let mut comparisons = match cfg.comparisons_jsonl {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| anyhow::anyhow!("failed to open comparisons jsonl {path}: {e}"))?;
            info!(path, "appending comparisons to JSONL");
            Some(std::io::BufWriter::new(file))
        }
        None => None,
    };

    let adapter =
        StepAdapter { solver: &solver, controller: &controller, timeout_ms: cfg.timeout_ms };

    // Establish a baseline applied state (N-1) before the first comparison.
    establish_baseline(&adapter).await?;

    let mut processed = 0u64;
    let mut total_trades = 0usize;
    let mut comparable_trades = 0usize;
    let mut skipped_blocks = 0u64;
    loop {
        if controller
            .peek_next_block()
            .await
            .is_none()
        {
            info!("block stream ended");
            break;
        }
        let Some(top_block) = adapter.current_block().await else {
            warn!("no applied block yet; advancing");
            adapter.advance().await?;
            continue;
        };
        let target = top_block + 1;

        let trades = match decode_block_when_available(&mut decoder, target).await {
            Ok(trades) => trades,
            Err(e) => {
                skipped_blocks += 1;
                telemetry::record_skipped_block();
                warn!(
                    block = target,
                    skipped_total = skipped_blocks,
                    "decode failed, skipping block: {e}"
                );
                adapter.advance().await?;
                continue;
            }
        };

        let start = Instant::now();
        // Snapshot token prices at top-of-block (N-1) for the headline metric and the top-of-block
        // USD valuation.
        let prices_top = snapshot_prices(&solver).await;
        let ranges = resolve_block_range(&adapter, &trades).await?;
        // resolve_block_range advanced the solver to back-of-block (N); snapshot again so the
        // back-of-block improvement is valued against the state it was solved at.
        let prices_back = snapshot_prices(&solver).await;
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
            telemetry::record_range(range, cfg.chain, &prices_top, &prices_back);
        }
        if let Some(writer) = comparisons.as_mut() {
            super::jsonl::write_comparisons(writer, &ranges, &prices_top, &prices_back);
        }
        let elapsed_s = start.elapsed().as_secs_f64();
        telemetry::record_block_seconds(elapsed_s);

        total_trades += ranges.len();
        comparable_trades += ranges
            .iter()
            .filter(|r| matches!(r.verdict, Verdict::Win | Verdict::Loss))
            .count();
        telemetry::record_coverage(total_trades, comparable_trades);

        info!(block = target, trades = ranges.len(), elapsed_s, "re-solved block (top/back)");

        processed += 1;
        if cfg
            .max_blocks
            .is_some_and(|max| processed >= max)
        {
            info!(processed, "reached --max-blocks");
            break;
        }
    }
    Ok(())
}

/// Release blocks until the solver has an applied market state, so the first comparison has a
/// genuine top-of-block (N-1) reference.
async fn establish_baseline(adapter: &StepAdapter<'_>) -> anyhow::Result<()> {
    if adapter.current_block().await.is_some() {
        return Ok(());
    }
    info!("waiting for solver to apply its first block…");
    adapter.advance().await?;
    Ok(())
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
        run(MonitorConfig {
            rpc_url: &rpc_url,
            tycho_url: &tycho_url,
            chain: "ethereum",
            protocols: vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
            // High TVL floor → fewer pools → faster load for a smoke test.
            min_tvl: 10_000.0,
            tycho_api_key: api_key.as_deref(),
            worker_pools_config: std::path::Path::new("worker_pools.toml"),
            timeout_ms: 10_000,
            metrics_port: None,
            max_blocks: Some(1),
            comparisons_jsonl: None,
        })
        .await
        .expect("monitor should process one block without error");
    }
}
