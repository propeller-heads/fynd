//! Live two-state monitor: drive an in-process `fynd-core` solver one block at a time, re-solving
//! each block's settled trades at top-of-block (N-1) and back-of-block (N).
//!
//! The block barrier is deterministic: after releasing a block via
//! [`BlockStepController::trigger_next_block`], we wait until the solver's `MarketData` reports the
//! next applied block before re-solving back-of-block. The pure orchestration is unit-tested in the
//! parent module via a mock [`SteppingSolver`]; this live driver is exercised by the gated
//! integration test in `tests/` (requires `TYCHO_URL` + `ETH_RPC_URL`).

use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use fynd_core::{
    types::{parse_chain, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest, QuoteStatus},
    BlockStepController, FyndBuilder, PoolConfig, Solver,
};
use num_bigint::BigUint;
use tracing::{info, warn};
use tycho_simulation::tycho_common::models::Address as CoreAddress;

use crate::{
    decoder::decode_block,
    resolve::{resolve_block_range, Outcome, SolvedAmount, SteppingSolver},
};

/// How long to wait for the solver to apply the next block after releasing it.
const BLOCK_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Inputs for the live monitor.
pub(crate) struct MonitorConfig<'a> {
    pub rpc_url: &'a str,
    pub tycho_url: &'a str,
    pub chain: &'a str,
    pub protocols: Vec<String>,
    pub min_tvl: f64,
    pub tycho_api_key: Option<&'a str>,
    pub timeout_ms: u64,
    pub metrics_port: Option<u16>,
    /// Stop after this many blocks (`None` runs until interrupted).
    pub max_blocks: Option<u64>,
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
        let order = Order::new(
            CoreAddress::from(token_in.into_array()),
            CoreAddress::from(token_out.into_array()),
            amount,
            OrderSide::Sell,
            CoreAddress::from([0u8; 20]),
        );
        let request =
            QuoteRequest::new(vec![order], QuoteOptions::default().with_timeout_ms(self.timeout_ms));

        match self.solver.quote(request).await {
            Ok(quote) => quote
                .orders()
                .first()
                .map(order_quote_to_outcome)
                .unwrap_or_else(|| Outcome::Unsolvable("solver returned no order quote".to_string())),
            Err(e) => Outcome::Unsolvable(format!("solve error: {e}")),
        }
    }

    async fn advance(&self) -> anyhow::Result<()> {
        let before = self.current_block().await;
        self.controller
            .trigger_next_block()
            .map_err(|_| anyhow::anyhow!("block stream ended"))?;

        // Deterministic barrier: wait until the solver applies a block strictly newer than `before`.
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
    Outcome::Solved(SolvedAmount {
        amount_out: biguint_to_u256(quote.amount_out()),
        amount_out_net_gas: biguint_to_u256(quote.amount_out_net_gas()),
        gas_estimate: biguint_to_u256(quote.gas_estimate()),
    })
}

fn biguint_to_u256(value: &BigUint) -> U256 {
    value
        .to_string()
        .parse()
        .unwrap_or(U256::ZERO)
}

/// Build the in-process stepped solver and re-solve each block's settled trades as a top/back range.
pub(crate) async fn run(cfg: MonitorConfig<'_>) -> anyhow::Result<()> {
    let chain = parse_chain(cfg.chain)
        .map_err(|e| anyhow::anyhow!("invalid --chain '{}': {e}", cfg.chain))?;

    info!(chain = cfg.chain, "building in-process solver (loading tokens may take minutes)…");
    let mut builder =
        FyndBuilder::new(chain, cfg.tycho_url, cfg.rpc_url, cfg.protocols.clone(), cfg.min_tvl);
    if let Some(key) = cfg.tycho_api_key {
        builder = builder.tycho_api_key(key);
    }
    let builder = builder
        .add_pool("hindsight", &PoolConfig::new("most_liquid"))
        .map_err(|e| anyhow::anyhow!("failed to configure worker pool: {e}"))?;
    let (solver, controller) = builder
        .build_with_step_controller()
        .await
        .map_err(|e| anyhow::anyhow!("failed to build solver: {e}"))?;

    let provider = crate::provider_from(cfg.rpc_url)?;

    if let Some(port) = cfg.metrics_port {
        crate::telemetry::install_exporter(port)?;
        info!(port, "serving Prometheus metrics at /metrics");
    }

    let adapter = StepAdapter { solver: &solver, controller: &controller, timeout_ms: cfg.timeout_ms };

    // Establish a baseline applied state (N-1) before the first comparison.
    establish_baseline(&adapter).await?;

    let mut processed = 0u64;
    loop {
        if controller.peek_next_block().await.is_none() {
            info!("block stream ended");
            break;
        }
        let Some(top_block) = adapter.current_block().await else {
            warn!("no applied block yet; advancing");
            adapter.advance().await?;
            continue;
        };
        let target = top_block + 1;

        let trades = match decode_block(&provider, target).await {
            Ok(trades) => trades,
            Err(e) => {
                warn!(block = target, "decode failed, skipping block: {e}");
                adapter.advance().await?;
                continue;
            }
        };

        let start = Instant::now();
        let ranges = resolve_block_range(&adapter, &trades).await?;
        for range in &ranges {
            crate::telemetry::record_range(range, cfg.chain);
        }
        let elapsed_s = start.elapsed().as_secs_f64();
        crate::telemetry::record_block_seconds(elapsed_s);
        info!(block = target, trades = ranges.len(), elapsed_s, "re-solved block (top/back)");

        processed += 1;
        if cfg.max_blocks.is_some_and(|max| processed >= max) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end smoke test of the live two-state monitor against a real solver.
    ///
    /// `#[ignore]`d so it never runs in CI (no Tycho/RPC). Run with:
    /// `TYCHO_URL=<ws> ETH_RPC_URL=<https> cargo test -p hindsight --bin hindsight \
    ///   resolve::monitor -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires live TYCHO_URL + ETH_RPC_URL"]
    async fn monitor_one_block_smoke() {
        let (Ok(rpc_url), Ok(tycho_url)) =
            (std::env::var("ETH_RPC_URL"), std::env::var("TYCHO_URL"))
        else {
            eprintln!("skipping: set ETH_RPC_URL and TYCHO_URL");
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
            timeout_ms: 10_000,
            metrics_port: None,
            max_blocks: Some(1),
        })
        .await
        .expect("monitor should process one block without error");
    }
}
