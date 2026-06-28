//! Re-solve engine: run Fynd on a decoded swap's inputs and compare its output against what
//! actually settled on-chain.
//!
//! The [`ReSolver`] trait abstracts the solver so the comparison pipeline is testable without a
//! live Fynd instance. The production implementation ([`run`]) re-solves through a running Fynd
//! via the shared `FyndAggregator` (HTTP). This compares at the chain's current state; re-solving
//! at top-of-block (N-1) and back-of-block (N) as a range is a follow-up that depends on
//! block-stepping support (`BlockStepController`) being wired into `fynd-core`.

mod compare;
pub(crate) mod monitor;
pub(crate) mod run;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use serde::Serialize;

pub(crate) use compare::{Deltas, Verdict};

use crate::decoder::DecodedTrade;

/// A Fynd quote for the re-solved order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SolvedAmount {
    pub amount_out: U256,
    /// Output after Fynd's own estimated gas cost.
    pub amount_out_net_gas: U256,
    pub gas_estimate: U256,
}

/// The outcome of re-solving a trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum Outcome {
    /// Fynd produced a quote.
    Solved(SolvedAmount),
    /// Fynd could not solve (missing token in Tycho, insufficient liquidity, timeout).
    Unsolvable(String),
}

/// Re-solves a sell order. Implementors return [`Outcome::Unsolvable`] (rather than an error)
/// when Fynd cannot produce a quote, so a single untradeable pair never aborts a whole block.
#[async_trait]
pub(crate) trait ReSolver {
    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome;
}

/// One re-solved trade compared against the settled on-chain amount.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Comparison {
    pub tx_hash: String,
    pub block_number: u64,
    pub client: String,
    pub aggregator: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub settled_amount_out: U256,
    pub outcome: Outcome,
    pub deltas: Deltas,
    pub verdict: Verdict,
}

/// Fynd's result at a single block state.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StateResult {
    pub outcome: Outcome,
    pub deltas: Deltas,
    pub verdict: Verdict,
}

impl StateResult {
    fn new(outcome: Outcome, settled_amount_out: U256) -> Self {
        let deltas = compare::compare(&outcome, settled_amount_out);
        let verdict = compare::verdict(&outcome, settled_amount_out);
        Self { outcome, deltas, verdict }
    }
}

/// A trade re-solved at both block states, presented as a range.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RangeComparison {
    pub tx_hash: String,
    pub block_number: u64,
    pub client: String,
    pub aggregator: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub settled_amount_out: U256,
    /// Optimistic: solved at state N-1, before the block's swaps moved the pools.
    pub top: StateResult,
    /// Pessimistic: solved at state N, after the block's swaps moved the pools.
    pub back: StateResult,
    /// Headline verdict — top-of-block (the optimistic default).
    pub verdict: Verdict,
}

/// Solves a sell order at the current block state and steps to the next block. The production
/// implementation ([`monitor`]) drives an in-process `fynd-core` solver via [`BlockStepController`];
/// tests use a mock returning a top- then back-of-block outcome.
#[async_trait]
pub(crate) trait SteppingSolver {
    /// Solve a sell order at the solver's current block state.
    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome;
    /// Release the held block and settle the solver onto the next block's state.
    async fn advance(&self) -> anyhow::Result<()>;
}

/// Build a [`RangeComparison`] from the two per-state outcomes of a trade.
pub(crate) fn build_range(trade: &DecodedTrade, top: Outcome, back: Outcome) -> RangeComparison {
    let top = StateResult::new(top, trade.amount_out);
    let back = StateResult::new(back, trade.amount_out);
    let verdict = top.verdict;
    RangeComparison {
        tx_hash: trade.tx_hash.clone(),
        block_number: trade.block_number,
        client: trade.client.clone(),
        aggregator: trade.aggregator.clone(),
        token_in: trade.token_in,
        token_out: trade.token_out,
        amount_in: trade.amount_in,
        settled_amount_out: trade.amount_out,
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
) -> anyhow::Result<Vec<RangeComparison>> {
    let mut tops = Vec::with_capacity(trades.len());
    for trade in trades {
        tops.push(solver.solve(trade.token_in, trade.token_out, trade.amount_in).await);
    }

    solver.advance().await?;

    let mut ranges = Vec::with_capacity(trades.len());
    for (trade, top) in trades.iter().zip(tops) {
        let back = solver
            .solve(trade.token_in, trade.token_out, trade.amount_in)
            .await;
        ranges.push(build_range(trade, top, back));
    }
    Ok(ranges)
}

/// Re-solve one decoded trade and compare it against the settled amount.
pub(crate) async fn compare_trade<R: ReSolver + ?Sized>(
    resolver: &R,
    trade: &DecodedTrade,
) -> Comparison {
    let outcome = resolver
        .solve(trade.token_in, trade.token_out, trade.amount_in)
        .await;
    let deltas = compare::compare(&outcome, trade.amount_out);
    let verdict = compare::verdict(&outcome, trade.amount_out);

    Comparison {
        tx_hash: trade.tx_hash.clone(),
        block_number: trade.block_number,
        client: trade.client.clone(),
        aggregator: trade.aggregator.clone(),
        token_in: trade.token_in,
        token_out: trade.token_out,
        amount_in: trade.amount_in,
        settled_amount_out: trade.amount_out,
        outcome,
        deltas,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock resolver returning a fixed outcome.
    struct MockSolver(Outcome);

    #[async_trait]
    impl ReSolver for MockSolver {
        async fn solve(&self, _: Address, _: Address, _: U256) -> Outcome {
            self.0.clone()
        }
    }

    fn trade(settled: u64) -> DecodedTrade {
        DecodedTrade {
            tx_hash: "0xabc".into(),
            block_number: 21_000_000,
            client: "relay".into(),
            aggregator: "tycho".into(),
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(settled),
        }
    }

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
        })
    }

    #[tokio::test]
    async fn compare_trade_win_carries_identity() {
        let cmp = compare_trade(&MockSolver(solved(10_200, 10_100)), &trade(10_000)).await;
        assert_eq!(cmp.verdict, Verdict::Win);
        assert_eq!(cmp.client, "relay");
        assert_eq!(cmp.settled_amount_out, U256::from(10_000u64));
        assert!(cmp.deltas.raw_bps.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn compare_trade_unsolvable() {
        let cmp = compare_trade(&MockSolver(Outcome::Unsolvable("no route".into())), &trade(10_000))
            .await;
        assert_eq!(cmp.verdict, Verdict::Unsolvable);
        assert_eq!(cmp.deltas, Deltas { raw_bps: None, net_bps: None });
    }

    #[tokio::test]
    async fn compare_trade_loss_when_gas_eats_edge() {
        // Raw better but net-of-gas worse → loss.
        let cmp = compare_trade(&MockSolver(solved(10_100, 9_980)), &trade(10_000)).await;
        assert_eq!(cmp.verdict, Verdict::Loss);
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
        let range = build_range(&trade(10_000), solved(10_200, 10_100), solved(10_010, 9_990));
        assert_eq!(range.verdict, Verdict::Win); // top is the headline
        assert!(range.top.deltas.raw_bps.unwrap() > range.back.deltas.raw_bps.unwrap());
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
        let ranges = resolve_block_range(&solver, &trades)
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
