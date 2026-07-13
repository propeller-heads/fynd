//! Re-solve engine: run Fynd on a decoded swap's inputs and compare its output against what
//! actually settled on-chain.
//!
//! The [`SteppingSolver`] trait abstracts the solver so the two-state comparison pipeline is
//! testable without a live Fynd instance. The production implementation ([`monitor`]) drives an
//! in-process `fynd-core` solver one block at a time, re-solving each trade at top-of-block (N-1)
//! and back-of-block (N).

mod compare;
mod jsonl;
pub(crate) mod monitor;

use alloy::primitives::{Address, TxHash, U256};
use async_trait::async_trait;
pub(crate) use compare::{Deltas, Verdict};
use serde::Serialize;

use crate::{
    decoder::{AttributionSource, DecodedTrade, SandwichEvidence, SolverQuote},
    usd,
};

/// A Fynd quote for the re-solved order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SolvedAmount {
    pub amount_out: U256,
    /// Output after Fynd's own estimated gas cost.
    pub amount_out_net_gas: U256,
    pub gas_estimate: U256,
    /// The complete serialized Fynd quote (route, per-hop pools/amounts, encoded transaction) for
    /// dumping improvements. `None` when not captured (e.g. the HTTP resolve path).
    #[serde(default)]
    pub quote_json: Option<String>,
}

/// The outcome of re-solving a trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum Outcome {
    /// Fynd produced a quote for the trade's full size.
    Solved(SolvedAmount),
    /// Fynd returned a route but for far less than the settled size — a liquidity-limited partial
    /// route. Tracked apart from [`Outcome::Unsolvable`] so a coverage gap is not read as a loss.
    Partial(String),
    /// Fynd could not solve at all (missing token in Tycho, insufficient liquidity, timeout).
    Unsolvable(String),
}

/// Fynd's result at a single block state.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StateResult {
    pub outcome: Outcome,
    pub deltas: Deltas,
    pub verdict: Verdict,
}

impl StateResult {
    fn new(outcome: Outcome, settled_amount_out: U256, settled_net_gas: U256) -> Self {
        let outcome = compare::served(outcome, settled_amount_out);
        let deltas = compare::compare(&outcome, settled_amount_out, settled_net_gas);
        let verdict = compare::verdict(&outcome, &deltas);
        Self { outcome, deltas, verdict }
    }
}

/// A trade re-solved at both block states, presented as a range.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RangeComparison {
    pub tx_hash: TxHash,
    pub block_number: u64,
    pub tx_index: u64,
    pub venue: String,
    pub solver: String,
    /// The evidence tier the solver label came from (from the decoder).
    pub solver_source: AttributionSource,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub settled_amount_out: U256,
    /// Settled output after the gas the trader paid for the route, in `token_out` units. Equals
    /// `settled_amount_out` when that gas is unknown, was paid by someone else, or the output
    /// token is unpriced.
    pub settled_amount_out_net_gas: U256,
    /// Wei cost of the settled route's gas, when the trader paid it (from the decoder).
    pub settled_gas: Option<U256>,
    /// The solver's own off-chain quote from its calldata, when declared (from the decoder).
    pub quote: Option<SolverQuote>,
    /// Evidence that a front-run and a back-run bracketed this trade (from the decoder). `None`
    /// when no bracket pair was found.
    pub sandwich: Option<SandwichEvidence>,
    /// Optimistic: solved at state N-1, before the block's swaps moved the pools.
    pub top: StateResult,
    /// Pessimistic: solved at state N, after the block's swaps moved the pools.
    pub back: StateResult,
    /// Headline verdict — top-of-block (the optimistic default).
    pub verdict: Verdict,
}

/// Solves a sell order at the current block state and steps to the next block. The production
/// implementation ([`monitor`]) drives an in-process `fynd-core` solver via
/// [`BlockStepController`]; tests use a mock returning a top- then back-of-block outcome.
#[async_trait]
pub(crate) trait SteppingSolver {
    /// Solve a sell order at the solver's current block state.
    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome;
    /// Release the held block and settle the solver onto the next block's state.
    async fn advance(&self) -> anyhow::Result<()>;
}

/// Build a [`RangeComparison`] from the two per-state outcomes of a trade.
///
/// When the decoder isolated the gas the trader paid for the settled route, its cost is converted
/// into `token_out` units at the `prices` snapshot (top-of-block — a fine approximation for a gas
/// deduction) and subtracted from the settled output, so both sides of the net comparison carry
/// their own gas.
///
/// When the decoder flagged the trade as sandwiched, both per-state verdicts (and therefore the
/// headline verdict) become [`Verdict::Sandwiched`] — the settled output was moved by MEV, not by
/// inferior routing — but the bps/USD deltas are left untouched, so the size of MEV-inflated
/// deltas stays studyable offline.
pub(crate) fn build_range(
    trade: &DecodedTrade,
    prices: &usd::PriceMap,
    top: Outcome,
    back: Outcome,
) -> RangeComparison {
    let settled_net_gas = trade
        .settled_gas
        .and_then(|gas| usd::gas_in_token(gas, trade.token_out, prices))
        .map_or(trade.amount_out, |gas_out| trade.amount_out.saturating_sub(gas_out));
    let mut top = StateResult::new(top, trade.amount_out, settled_net_gas);
    let mut back = StateResult::new(back, trade.amount_out, settled_net_gas);
    if trade.sandwich.is_some() {
        top.verdict = Verdict::Sandwiched;
        back.verdict = Verdict::Sandwiched;
    }
    let verdict = top.verdict;
    RangeComparison {
        tx_hash: trade.tx_hash,
        block_number: trade.block_number,
        tx_index: trade.tx_index,
        venue: trade.venue.clone(),
        solver: trade.solver.clone(),
        solver_source: trade.solver_source,
        token_in: trade.token_in,
        token_out: trade.token_out,
        amount_in: trade.amount_in,
        settled_amount_out: trade.amount_out,
        settled_amount_out_net_gas: settled_net_gas,
        settled_gas: trade.settled_gas,
        quote: trade.quote.clone(),
        sandwich: trade.sandwich.clone(),
        top,
        back,
        verdict,
    }
}

/// Re-solve every trade in a held block at top-of-block, advance to back-of-block, re-solve again,
/// and pair the results. Solving all trades at one state before advancing keeps each state's reads
/// consistent and steps the chain only once per block.
pub(crate) async fn resolve_block_range<S: SteppingSolver + ?Sized>(
    solver: &S,
    trades: &[DecodedTrade],
    prices: &usd::PriceMap,
) -> anyhow::Result<Vec<RangeComparison>> {
    let mut tops = Vec::with_capacity(trades.len());
    for trade in trades {
        tops.push(
            solver
                .solve(trade.token_in, trade.token_out, trade.amount_in)
                .await,
        );
    }

    solver.advance().await?;

    let mut ranges = Vec::with_capacity(trades.len());
    for (trade, top) in trades.iter().zip(tops) {
        let back = solver
            .solve(trade.token_in, trade.token_out, trade.amount_in)
            .await;
        ranges.push(build_range(trade, prices, top, back));
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::TxHash;

    use super::*;

    fn trade(settled: u64) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 21_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "tycho".into(),
            solver_source: AttributionSource::TraceMatch,
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(settled),
            venue_fee: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        }
    }

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            quote_json: None,
        })
    }

    /// Stepping mock: returns `top` before `advance()`, `back` after.
    struct MockStepping {
        advanced: std::sync::atomic::AtomicBool,
        top: Outcome,
        back: Outcome,
    }

    #[async_trait]
    impl SteppingSolver for MockStepping {
        async fn solve(&self, _: Address, _: Address, _: U256) -> Outcome {
            if self
                .advanced
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.back.clone()
            } else {
                self.top.clone()
            }
        }

        async fn advance(&self) -> anyhow::Result<()> {
            self.advanced
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn build_range_headline_is_top() {
        let range = build_range(
            &trade(10_000),
            &usd::PriceMap::new(),
            solved(10_200, 10_100),
            solved(10_010, 9_990),
        );
        assert_eq!(range.verdict, Verdict::Win); // top is the headline
        assert!(range.top.deltas.raw_bps.unwrap() > range.back.deltas.raw_bps.unwrap());
    }

    #[test]
    fn build_range_partial_fill_is_coverage_miss() {
        // Fynd fills only 10% of a 10_000 settled trade → reclassified as a coverage miss.
        let range = build_range(
            &trade(10_000),
            &usd::PriceMap::new(),
            solved(1_000, 990),
            solved(1_000, 990),
        );
        assert_eq!(range.verdict, Verdict::CoverageMiss);
        assert_eq!(range.top.deltas, Deltas { raw_bps: None, net_bps: None });
        assert!(matches!(range.top.outcome, Outcome::Partial(_)));
    }

    #[test]
    fn build_range_sandwiched_trade_overrides_verdict_but_keeps_deltas() {
        let mut sandwiched = trade(10_000);
        sandwiched.sandwich = Some(SandwichEvidence {
            front_tx: TxHash::repeat_byte(0xaa),
            back_tx: TxHash::repeat_byte(0xbb),
            attacker: Address::repeat_byte(0xcc),
            pools: vec![Address::repeat_byte(0xdd)],
        });
        let range = build_range(
            &sandwiched,
            &usd::PriceMap::new(),
            solved(10_200, 10_100),
            solved(9_800, 9_700),
        );

        assert_eq!(range.verdict, Verdict::Sandwiched);
        assert_eq!(range.top.verdict, Verdict::Sandwiched);
        assert_eq!(range.back.verdict, Verdict::Sandwiched);
        // Deltas are unaffected by the override: still computed for offline analysis.
        assert!(range.top.deltas.raw_bps.unwrap() > 0.0);
        assert!(range.back.deltas.raw_bps.unwrap() < 0.0);
    }

    #[test]
    fn build_range_deducts_settled_gas_when_priced() {
        // The settled trader paid 200 token_out units of gas (100 wei at a price of 2 units/wei):
        // the secondary net column carries the deduction; the verdict stays gross vs gross.
        let mut with_gas = trade(10_000);
        with_gas.settled_gas = Some(U256::from(100u64));
        let prices = usd::PriceMap::from([(with_gas.token_out, 2.0)]);

        let range = build_range(&with_gas, &prices, solved(10_050, 9_990), solved(10_050, 9_990));
        assert_eq!(range.settled_amount_out_net_gas, U256::from(9_800u64));
        assert_eq!(range.settled_amount_out, U256::from(10_000u64));
        assert_eq!(range.verdict, Verdict::Win);
    }

    #[test]
    fn build_range_unpriced_gas_keeps_settled_gross() {
        // token_out is not in the price map → no deduction. The secondary net column stays
        // gross; the verdict is unaffected either way (gross 10_050 beats gross 10_000).
        let mut with_gas = trade(10_000);
        with_gas.settled_gas = Some(U256::from(100u64));

        let range = build_range(
            &with_gas,
            &usd::PriceMap::new(),
            solved(10_050, 9_990),
            solved(10_050, 9_990),
        );
        assert_eq!(range.settled_amount_out_net_gas, U256::from(10_000u64));
        assert_eq!(range.verdict, Verdict::Win);
    }

    #[tokio::test]
    async fn resolve_block_range_pairs_top_and_back() {
        // Two trades. Top-of-block is optimistic (better), back-of-block pessimistic (worse).
        let solver = MockStepping {
            advanced: std::sync::atomic::AtomicBool::new(false),
            top: solved(10_200, 10_100),
            back: solved(9_900, 9_800),
        };
        let trades = [trade(10_000), trade(10_000)];
        let ranges = resolve_block_range(&solver, &trades, &usd::PriceMap::new())
            .await
            .unwrap();

        assert_eq!(ranges.len(), 2);
        for range in &ranges {
            assert_eq!(range.top.verdict, Verdict::Win);
            assert_eq!(range.back.verdict, Verdict::Loss);
            assert!(range.top.deltas.raw_bps.unwrap() > range.back.deltas.raw_bps.unwrap());
        }
    }
}
