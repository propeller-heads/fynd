//! Block-batch capture for the offline APEX batching study.
//!
//! The monitor already decodes every settled trade in a block and re-solves it against Fynd at
//! top-of-block. That is exactly the input an APEX batch needs, minus one thing: the market state
//! itself, which is captured separately as a `record-market` recording (raw tycho `Update`
//! messages, replayed 0..=k to rebuild block k). So this module persists the *orders* half — one
//! JSON line per block carrying every decoded trade, the Fynd counterfactual it was measured
//! against, the order's `min_amount_out`, and the solver's token-price map at N-1 — and the
//! offline runner (`tools/apex-batch`) joins the two.
//!
//! Capture is append-only and cheap: it reuses the monitor's own per-block work, so turning it on
//! costs one serialization per block and never changes what the monitor measures.
//!
//! The record shape below is the contract with `tools/apex-batch/src/snapshot.rs`, which
//! deserializes its own mirror of these fields. Renaming a field here is a breaking change there.

use std::collections::BTreeMap;

use alloy::primitives::U256;
use serde::Serialize;
use tracing::warn;

use crate::{
    decoder::DecodedTrade,
    resolve::{Outcome, RangeComparison},
    usd::Prices,
};

/// Where an order's `min_amount_out` came from — the honesty flag on every captured limit.
///
/// An extracted limit is the real floor the settling solver committed to on-chain. A synthetic one
/// is derived from the executed output and a slippage assumption, which biases the study: too
/// loose a synthetic limit inflates APEX's measured surplus. Splitting the results by this field
/// is how that bias is bounded rather than assumed away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[expect(dead_code, reason = "scaffold: constructed once limit_for lands")]
pub(crate) enum LimitSource {
    /// Decoded from the settled transaction's own calldata (`CoW`'s order `buyAmount`, a router's
    /// `minReturnAmount`).
    Extracted,
    /// Derived from the executed output because the solver's calldata declares no floor.
    Synthetic,
}

/// Fynd's top-of-block counterfactual for one trade, flattened to what the batch runner scores
/// against. This is the study's baseline: "what would Fynd have delivered for this order alone,
/// at state N-1" — the number APEX's batch clearing has to beat.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FyndCounterfactual {
    /// `solved`, `partial`, or `unsolvable` — mirrors [`Outcome`]'s discriminant so an
    /// unsolvable baseline is distinguishable from a zero one.
    pub status: &'static str,
    /// Gross output Fynd quoted, in `token_out` native units. `None` unless solved.
    pub amount_out: Option<String>,
    /// Output after Fynd's own estimated gas cost. `None` unless solved.
    pub amount_out_net_gas: Option<String>,
    /// Which worker pool won the quote, for per-algorithm splits of the baseline.
    pub algorithm: Option<String>,
    /// Why Fynd could not serve the trade — the coverage-gap signal, kept so an excluded order
    /// can be attributed to Fynd's coverage rather than to the batch.
    pub unsolvable_reason: Option<String>,
}

/// One settled trade as an APEX batch order: the decoded swap, its limit, and the Fynd
/// counterfactual it is scored against.
///
/// Amounts are decimal strings, not JSON numbers: `U256` does not survive an `f64` round trip and
/// the runner needs the exact settled amounts to compute surplus in basis points.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CapturedTrade {
    pub tx_hash: String,
    /// Position in the block — the batch's "original position", kept for the deferred
    /// original-positions variant even though the current matrix only uses top and biased bottom.
    pub tx_index: u64,
    pub venue: String,
    pub solver: String,
    /// Which decoder recovered the trade, so a decoder-specific bias in the batch results is
    /// visible instead of pooled.
    pub decoder: &'static str,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
    pub settled_amount_out: String,
    /// Settled output after the gas the trader paid for the route.
    pub settled_amount_out_net_gas: String,
    /// The order's floor: the minimum output the batch must deliver for this order to clear.
    /// `None` when no floor could be extracted *and* no synthetic one was applied, which excludes
    /// the order from the batch (counted as `LimitUnextractable` by the runner).
    pub min_amount_out: Option<String>,
    /// Provenance of `min_amount_out`. `None` exactly when `min_amount_out` is.
    pub limit_source: Option<LimitSource>,
    /// Whether MEV bracketed this trade. A sandwiched settled output is not a fair baseline, so
    /// the analysis reports these separately rather than dropping them silently.
    pub sandwiched: bool,
    pub fynd_top: FyndCounterfactual,
}

/// Everything one block contributes to the batching study: the orders and the price view they
/// were solved under.
///
/// Market state is deliberately absent — it lives in the parallel `record-market` recording,
/// which is both far larger and shared across every block. The runner joins on `block`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BlockBatchSnapshot {
    pub block: u64,
    /// Fynd's derived per-token price at N-1, keyed by lowercase `0x` address: the token's native
    /// units per wei of the gas token. APEX needs absolute per-token prices as its tâtonnement
    /// starting point, and this is the same view the monitor valued the block in — so the
    /// baseline and the batch are priced identically.
    ///
    /// A `BTreeMap` so the line is byte-stable across runs and two captures of the same block
    /// diff cleanly.
    pub token_prices: BTreeMap<String, f64>,
    pub trades: Vec<CapturedTrade>,
}

/// Build one block's snapshot from the monitor's own per-block work: the decoded trades, the
/// re-solved ranges they produced, and the top-of-block price snapshot.
pub(crate) fn build_snapshot(
    block: u64,
    trades: &[DecodedTrade],
    ranges: &[RangeComparison],
    prices_top: &Prices,
) -> BlockBatchSnapshot {
    let token_prices = prices_top
        .iter()
        .map(|(token, price)| (format!("{token:#x}"), price))
        .collect();
    BlockBatchSnapshot { block, token_prices, trades: captured_trades(trades, ranges) }
}

/// Project each re-solved range to a batch order, taking the order's limit from the decoded trade
/// it came from.
///
/// The range already carries every decoded field the batch needs except `min_amount_out`, so the
/// join on transaction hash exists only to reach that one field. A range whose trade is missing
/// cannot be given a limit and is dropped rather than captured limitless — the runner would only
/// exclude it again, and a dropped order is visible in the per-block count.
fn captured_trades(trades: &[DecodedTrade], ranges: &[RangeComparison]) -> Vec<CapturedTrade> {
    let mut captured = Vec::with_capacity(ranges.len());
    for range in ranges {
        let Some(trade) = trades
            .iter()
            .find(|trade| trade.tx_hash == range.tx_hash)
        else {
            warn!(
                block = range.block_number,
                tx = %range.tx_hash,
                "re-solved range has no decoded trade; skipping its batch order"
            );
            continue;
        };
        let (min_amount_out, limit_source) = limit_for(trade);
        captured.push(CapturedTrade {
            tx_hash: format!("{:#x}", range.tx_hash),
            tx_index: range.tx_index,
            venue: range.venue.clone(),
            solver: range.solver.clone(),
            decoder: range.decoder,
            token_in: format!("{:#x}", range.token_in),
            token_out: format!("{:#x}", range.token_out),
            amount_in: range.amount_in.to_string(),
            settled_amount_out: range.settled_amount_out.to_string(),
            settled_amount_out_net_gas: range
                .settled_amount_out_net_gas
                .to_string(),
            min_amount_out: min_amount_out.map(|limit| limit.to_string()),
            limit_source,
            sandwiched: range.sandwich.is_some(),
            fynd_top: counterfactual(&range.top.outcome),
        });
    }
    captured
}

/// The order's floor and where it came from.
///
/// Should: prefer the trade's extracted `min_amount_out` (decoded from the settling solver's
/// calldata); when absent, fall back to the synthetic limit — the executed output less
/// [`SYNTHETIC_LIMIT_BPS`] — and label it as such. Returns `(None, None)` only when neither is
/// derivable, which is what the runner counts as an unextractable limit.
#[expect(clippy::todo, reason = "scaffold: the limit policy lands with the capture implementation")]
fn limit_for(_trade: &DecodedTrade) -> (Option<U256>, Option<LimitSource>) {
    todo!("return the extracted limit, else the synthetic executed-output-minus-slippage floor")
}

/// Slippage assumed for the synthetic fallback limit, in basis points below the executed output.
///
/// 100 bps is the dominant discrete preset across the venues we decode (1inch v6 swap medians
/// ~83 bps, unoswap ~200, `KyberSwap` ~151; pooled p50 ≈ 120). Deliberately on the tight side of
/// the pooled median: a too-loose limit lets the batch claim surplus the real order would never
/// have accepted, which is the one error direction that flatters APEX.
#[expect(dead_code, reason = "scaffold: read once limit_for lands")]
pub(crate) const SYNTHETIC_LIMIT_BPS: u32 = 100;

/// Flatten a re-solved state's outcome into the captured counterfactual.
fn counterfactual(outcome: &Outcome) -> FyndCounterfactual {
    match outcome {
        Outcome::Solved(solved) => FyndCounterfactual {
            status: "solved",
            amount_out: Some(solved.amount_out.to_string()),
            amount_out_net_gas: Some(solved.amount_out_net_gas.to_string()),
            algorithm: Some(solved.algorithm.clone()),
            unsolvable_reason: None,
        },
        Outcome::Partial(reason) => FyndCounterfactual {
            status: "partial",
            amount_out: None,
            amount_out_net_gas: None,
            algorithm: None,
            unsolvable_reason: Some(reason.clone()),
        },
        Outcome::Unsolvable(reason) => FyndCounterfactual {
            status: "unsolvable",
            amount_out: None,
            amount_out_net_gas: None,
            algorithm: None,
            unsolvable_reason: Some(reason.clone()),
        },
    }
}

/// Append one JSON line for `snapshot`. A block with no decodable trades is still written — the
/// runner needs to tell "no orders in this block" apart from "this block was never captured",
/// which is the difference between a zero and a gap in the per-block surplus series.
pub(crate) fn write_snapshot<W: std::io::Write>(writer: &mut W, snapshot: &BlockBatchSnapshot) {
    let Ok(line) = serde_json::to_string(snapshot) else {
        warn!(block = snapshot.block, "failed to serialize block batch snapshot");
        return;
    };
    if let Err(e) = writeln!(writer, "{line}") {
        warn!(block = snapshot.block, error = %e, "failed to write block batch snapshot");
        return;
    }
    if let Err(e) = writer.flush() {
        warn!(block = snapshot.block, error = %e, "failed to flush block batch snapshot");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decoder::Registry, resolve::SolvedAmount};

    #[test]
    fn test_counterfactual_distinguishes_unsolved_from_zero() {
        let solved = counterfactual(&Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_010u64),
            amount_out_net_gas: U256::from(1_005u64),
            gas_estimate: U256::from(21_000u64),
            algorithm: "most_liquid".to_string(),
            quote_json: None,
            solved_route: None,
        }));
        assert_eq!(solved.status, "solved");
        assert_eq!(solved.amount_out.as_deref(), Some("1010"));
        assert_eq!(solved.algorithm.as_deref(), Some("most_liquid"));
        assert!(solved.unsolvable_reason.is_none());

        // An unsolvable baseline must not serialize as a zero output: zero is a real APEX result
        // (the order did not clear), unsolvable means Fynd had no route at all.
        let unsolved = counterfactual(&Outcome::Unsolvable("missing token in Tycho".to_string()));
        assert_eq!(unsolved.status, "unsolvable");
        assert!(unsolved.amount_out.is_none());
        assert_eq!(unsolved.unsolvable_reason.as_deref(), Some("missing token in Tycho"));
    }

    #[test]
    fn test_snapshot_line_carries_the_price_view() {
        let usdc: alloy::primitives::Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let mut prices = Prices::new(&Registry::ethereum());
        prices.insert(usdc, 2e-9);

        let snapshot = BlockBatchSnapshot {
            block: 25_000_000,
            token_prices: prices
                .iter()
                .map(|(token, price)| (format!("{token:#x}"), price))
                .collect(),
            trades: Vec::new(),
        };
        let mut buf = Vec::new();
        write_snapshot(&mut buf, &snapshot);
        let line = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();

        assert_eq!(parsed.pointer("/block").unwrap(), 25_000_000);
        assert!(
            (parsed
                .pointer(&format!("/token_prices/{usdc:#x}"))
                .unwrap()
                .as_f64()
                .unwrap() -
                2e-9)
                .abs() <
                1e-18
        );
        // A block with no decodable trades is a captured zero, not a gap.
        assert_eq!(
            parsed
                .pointer("/trades")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }
}
