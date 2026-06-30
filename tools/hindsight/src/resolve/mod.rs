//! Re-solve engine: run Fynd on a decoded swap's inputs and compare its output against what
//! actually settled on-chain.
//!
//! The [`ReSolver`] trait abstracts the solver so the comparison pipeline is testable without a
//! live Fynd instance. The production implementation ([`run`]) re-solves through a running Fynd
//! via the shared `FyndAggregator` (HTTP). This compares at the chain's current state; re-solving
//! at top-of-block (N-1) and back-of-block (N) as a range is a follow-up that depends on
//! block-stepping support (`BlockStepController`) being wired into `fynd-core`.

mod compare;
pub(crate) mod run;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
pub(crate) use compare::{Deltas, Verdict};
use serde::Serialize;

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

/// Re-solve one decoded trade and compare it against the settled amount.
pub(crate) async fn compare_trade<R: ReSolver + ?Sized>(
    resolver: &R,
    trade: &DecodedTrade,
) -> Comparison {
    let outcome = resolver
        .solve(trade.token_in, trade.token_out, trade.amount_in)
        .await;
    let outcome = compare::served(outcome, trade.amount_out);
    let deltas = compare::compare(&outcome, trade.amount_out);
    let verdict = compare::verdict(&outcome, trade.amount_out);

    Comparison {
        tx_hash: trade.tx_hash.to_string(),
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
            tx_hash: Default::default(),
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
        let cmp =
            compare_trade(&MockSolver(Outcome::Unsolvable("no route".into())), &trade(10_000))
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
}
