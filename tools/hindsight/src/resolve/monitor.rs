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
    types::{
        parse_chain, EncodingOptions, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest,
        QuoteStatus, Swap, Transaction,
    },
    BlockStepController, FyndBuilder, PoolConfig, Solver,
};
use num_bigint::BigUint;
use tracing::{info, warn};
use tycho_simulation::tycho_common::models::Address as CoreAddress;

use crate::{
    decoder::decode_block,
    resolve::{
        resolve_block_range, Outcome, RangeComparison, SolvedAmount, StateResult, SteppingSolver,
        Verdict,
    },
    usd,
};

/// How long to wait for the solver to apply the next block after releasing it. Generous because the
/// Tycho stream periodically goes silent for minutes while it reconnects/resyncs the large
/// `all_onchain` synchronizer set; the stream recovers on its own, so the monitor should wait it
/// out rather than exit (which would reset all metrics). Only a truly dead feed should ever hit
/// this.
const BLOCK_SETTLE_TIMEOUT: Duration = Duration::from_secs(1800);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Inputs for the live monitor.
pub(crate) struct MonitorConfig<'a> {
    pub rpc_url: &'a str,
    pub tycho_url: &'a str,
    pub chain: &'a str,
    pub protocols: Vec<String>,
    pub min_tvl: f64,
    pub tycho_api_key: Option<&'a str>,
    /// Worker-pools TOML config path; defaults to a single `most_liquid` pool when absent.
    pub worker_pools_config: Option<&'a str>,
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
    let quote_json = serde_json::to_string(&slim_quote(quote)).ok();
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
    builder = match cfg.worker_pools_config {
        Some(path) => {
            let config = fynd_rpc::config::WorkerPoolsConfig::load_from_file(path)
                .map_err(|e| anyhow::anyhow!("failed to load worker pools config {path}: {e}"))?;
            let mut builder = builder;
            for (name, pool) in config.pools() {
                builder = builder
                    .add_pool(name, pool)
                    .map_err(|e| anyhow::anyhow!("failed to add worker pool {name}: {e}"))?;
            }
            builder
        }
        None => builder
            .add_pool("hindsight", &PoolConfig::new("most_liquid"))
            .map_err(|e| anyhow::anyhow!("failed to configure worker pool: {e}"))?,
    };
    let (solver, controller) = builder
        .build_with_step_controller()
        .await
        .map_err(|e| anyhow::anyhow!("failed to build solver: {e}"))?;

    let provider = crate::provider_from(cfg.rpc_url)?;

    if let Some(port) = cfg.metrics_port {
        crate::telemetry::install_exporter(port)?;
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

        let trades = match decode_block(&provider, target).await {
            Ok(trades) => trades,
            Err(e) => {
                warn!(block = target, "decode failed, skipping block: {e}");
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
        for range in &ranges {
            crate::telemetry::record_range(range, cfg.chain, &prices_top);
        }
        if let Some(writer) = comparisons.as_mut() {
            write_comparisons(writer, &ranges, &prices_top, &prices_back);
        }
        let elapsed_s = start.elapsed().as_secs_f64();
        crate::telemetry::record_block_seconds(elapsed_s);

        total_trades += ranges.len();
        comparable_trades += ranges
            .iter()
            .filter(|r| matches!(r.verdict, Verdict::Win | Verdict::Loss))
            .count();
        crate::telemetry::record_coverage(total_trades, comparable_trades);

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

/// Append one JSON line per re-solved trade to `writer` — every comparison, not just wins. Each
/// record carries both block states with their verdict (win/loss/unsolvable), so downstream can
/// filter to wins for the improvement view or to unsolvables for the coverage worklist (where Fynd
/// needs to improve). Losses keep their route (what path Fynd took and lost on); unsolvables keep
/// the reason.
fn write_comparisons<W: std::io::Write>(
    writer: &mut W,
    ranges: &[RangeComparison],
    prices_top: &usd::PriceMap,
    prices_back: &usd::PriceMap,
) {
    for range in ranges {
        let Ok(line) = serde_json::to_string(&comparison_record(range, prices_top, prices_back))
        else {
            continue;
        };
        if let Err(e) = writeln!(writer, "{line}") {
            warn!(error = %e, "failed to write comparison record");
            return;
        }
    }
    if let Err(e) = writer.flush() {
        warn!(error = %e, "failed to flush comparisons writer");
    }
}

/// Build the JSON record for one re-solved trade: block, settled tx, decoded amounts, and a `top`
/// and `back` state (each with its verdict, bps, USD delta, and slim route/calldata or unsolvable
/// reason). Top is valued at N-1 prices, back at N prices, matching the state each was solved at.
fn comparison_record(
    range: &RangeComparison,
    prices_top: &usd::PriceMap,
    prices_back: &usd::PriceMap,
) -> serde_json::Value {
    serde_json::json!({
        "block": range.block_number,
        "settled_tx": range.tx_hash,
        "client": range.client,
        "aggregator": range.aggregator,
        "token_in": format!("{:#x}", range.token_in),
        "token_out": format!("{:#x}", range.token_out),
        "amount_in": range.amount_in.to_string(),
        "settled_amount_out": range.settled_amount_out.to_string(),
        "relay_fill": range.relay_fill,
        "top": state_record(&range.top, range.token_out, range.settled_amount_out, prices_top),
        "back": state_record(&range.back, range.token_out, range.settled_amount_out, prices_back),
    })
}

/// JSON for one block-state of an improvement: verdict, bps, Fynd amounts, the USD improvement
/// (net-of-gas Fynd output minus the settled output, valued at `prices`), and the slim quote.
fn state_record(
    state: &StateResult,
    token_out: Address,
    settled_amount_out: U256,
    prices: &usd::PriceMap,
) -> serde_json::Value {
    let solved = match &state.outcome {
        Outcome::Solved(solved) => Some(solved),
        Outcome::Partial(_) | Outcome::Unsolvable(_) => None,
    };
    // The reason Fynd could not serve the trade — the coverage-gap signal (missing token,
    // insufficient liquidity, timeout, partial-fill coverage miss).
    let unsolvable_reason = match &state.outcome {
        Outcome::Unsolvable(reason) => Some(reason.as_str()),
        Outcome::Solved(_) => None,
    };
    let improvement_usd = solved.and_then(|s| {
        usd::savings_usd(token_out, s.amount_out_net_gas, settled_amount_out, prices)
    });
    let fynd_value_usd = solved.and_then(|s| usd::value_usd(token_out, s.amount_out, prices));
    serde_json::json!({
        "verdict": state.verdict,
        "net_bps": state.deltas.net_bps,
        "raw_bps": state.deltas.raw_bps,
        "fynd_amount_out": solved.map(|s| s.amount_out.to_string()),
        "fynd_amount_out_net_gas": solved.map(|s| s.amount_out_net_gas.to_string()),
        "gas_estimate": solved.map(|s| s.gas_estimate.to_string()),
        "improvement_usd": improvement_usd,
        "fynd_value_usd": fynd_value_usd,
        "settled_value_usd": usd::value_usd(token_out, settled_amount_out, prices),
        "unsolvable_reason": unsolvable_reason,
        "quote": solved
            .and_then(|s| s.quote_json.as_deref())
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok()),
    })
}

/// Project an `OrderQuote` down to what an investigation needs: order id, status, the encoded
/// transaction (calldata), and a per-hop route (protocol, pool, tokens, amounts, gas). Built from
/// the quote object's accessors so it never touches each hop's `protocol_state` — which is both
/// the bulk of the size and unserializable for vm pools (Curve etc.).
fn slim_quote(quote: &OrderQuote) -> serde_json::Value {
    let route: Vec<serde_json::Value> = quote
        .route()
        .map(|route| {
            route
                .swaps()
                .iter()
                .map(slim_swap)
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "order_id": quote.order_id(),
        "status": serde_json::to_value(quote.status()).ok(),
        "transaction": quote.transaction().map(slim_transaction),
        "route": route,
    })
}

/// One route hop: protocol, pool (the component id is the pool address), tokens, amounts, gas.
fn slim_swap(swap: &Swap) -> serde_json::Value {
    serde_json::json!({
        "protocol": swap.protocol(),
        "pool": swap.component_id(),
        "token_in": serde_json::to_value(swap.token_in()).ok(),
        "token_out": serde_json::to_value(swap.token_out()).ok(),
        "amount_in": swap.amount_in().to_string(),
        "amount_out": swap.amount_out().to_string(),
        "gas_estimate": swap.gas_estimate().to_string(),
        "split": swap.split(),
    })
}

/// The encoded on-chain transaction: target, native value, and hex calldata.
fn slim_transaction(transaction: &Transaction) -> serde_json::Value {
    serde_json::json!({
        "to": serde_json::to_value(transaction.to()).ok(),
        "value": transaction.value().to_string(),
        "data": format!("0x{}", alloy::hex::encode(transaction.data())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slim_transaction_emits_hex_calldata_and_address() {
        use tycho_simulation::tycho_common::Bytes;
        let tx = Transaction::new(
            Bytes::from(vec![0x11u8; 20]),
            BigUint::from(5u8),
            vec![0xde, 0xad, 0xbe, 0xef],
        );
        let slim = slim_transaction(&tx);
        assert_eq!(slim.get("data").unwrap(), "0xdeadbeef");
        assert_eq!(slim.get("value").unwrap(), "5");
        assert!(slim
            .get("to")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("0x"));
    }

    #[test]
    fn improvement_record_carries_top_and_back_with_usd_and_slim_route() {
        use crate::resolve::build_range;

        let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let weth: Address = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            .parse()
            .unwrap();
        // ETH=$2000: USDC (6dp) = 2e-9 native units/wei, WETH (18dp) = 1.0.
        let prices = usd::PriceMap::from([(usdc, 2e-9), (weth, 1.0)]);

        let trade = crate::decoder::DecodedTrade {
            tx_hash: "0xabc".into(),
            block_number: 25_000_000,
            client: "relay".into(),
            aggregator: "1inch".into(),
            sender: Address::ZERO,
            token_in: weth,
            token_out: usdc,
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000_000_000u64), // settled 1000 USDC
            client_fee: None,
            relay_fill: None,
        };
        // quote_json is already the slim projection (what order_quote_to_outcome stores).
        let quote = Some(
            r#"{"order_id":"o","status":"success","transaction":{"to":"0xrouter","value":"0",
                "data":"0x01"},"route":[{"protocol":"uniswap_v3","pool":"0xpool",
                "token_in":"0xaaa","token_out":"0xbbb","amount_in":"1","amount_out":"2",
                "gas_estimate":"0","split":1.0}]}"#
                .to_string(),
        );
        // Top: net 1005 USDC → +$5. Back: net 1001 USDC → +$1. Both win.
        let top = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_010_000_000u64),
            amount_out_net_gas: U256::from(1_005_000_000u64),
            gas_estimate: U256::from(21_000u64),
            quote_json: quote.clone(),
        });
        let back = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_002_000_000u64),
            amount_out_net_gas: U256::from(1_001_000_000u64),
            gas_estimate: U256::from(21_000u64),
            quote_json: quote,
        });
        let range = build_range(&trade, top, back);

        let rec = comparison_record(&range, &prices, &prices);
        let top_usd = rec
            .pointer("/top/improvement_usd")
            .unwrap()
            .as_f64()
            .unwrap();
        let back_usd = rec
            .pointer("/back/improvement_usd")
            .unwrap()
            .as_f64()
            .unwrap();
        assert!((top_usd - 5.0).abs() < 1e-3, "top_usd={top_usd}");
        assert!((back_usd - 1.0).abs() < 1e-3, "back_usd={back_usd}");
        assert!(
            rec.pointer("/back/net_bps")
                .unwrap()
                .as_f64()
                .unwrap() >
                0.0
        );
        // Both states embed the slim quote: calldata and route/pool are present.
        assert_eq!(
            rec.pointer("/top/quote/transaction/data")
                .unwrap(),
            "0x01"
        );
        assert_eq!(
            rec.pointer("/top/quote/route/0/pool")
                .unwrap(),
            "0xpool"
        );
        assert_eq!(
            rec.pointer("/back/quote/route/0/protocol")
                .unwrap(),
            "uniswap_v3"
        );
    }

    #[test]
    fn comparison_record_captures_unsolvable_reason_and_null_quote() {
        use crate::resolve::build_range;
        let trade = crate::decoder::DecodedTrade {
            tx_hash: "0xabc".into(),
            block_number: 25_000_000,
            client: "relay".into(),
            aggregator: "1inch".into(),
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000u64),
            client_fee: None,
            relay_fill: None,
        };
        // A coverage gap: Fynd could not solve at either state.
        let range = build_range(
            &trade,
            Outcome::Unsolvable("missing token in Tycho".into()),
            Outcome::Unsolvable("missing token in Tycho".into()),
        );
        let rec = comparison_record(&range, &usd::PriceMap::new(), &usd::PriceMap::new());
        assert_eq!(rec.pointer("/top/verdict").unwrap(), "unsolvable");
        assert_eq!(
            rec.pointer("/top/unsolvable_reason")
                .unwrap(),
            "missing token in Tycho"
        );
        assert!(rec
            .pointer("/top/quote")
            .unwrap()
            .is_null());
    }

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
            worker_pools_config: None,
            timeout_ms: 10_000,
            metrics_port: None,
            max_blocks: Some(1),
            comparisons_jsonl: None,
        })
        .await
        .expect("monitor should process one block without error");
    }
}
