//! Pure comparison math between a Fynd re-solve and the on-chain settled amount.

use alloy::primitives::U256;
use fynd_tools_common::bps::raw_bps_diff;
use num_bigint::BigUint;
use serde::Serialize;

use crate::{resolve::Outcome, usd};

/// Convert an alloy `U256` token amount to `BigUint` for the bps helpers, big-endian and without a
/// decimal string round-trip.
fn to_biguint(amount: U256) -> BigUint {
    BigUint::from_bytes_be(&amount.to_be_bytes::<32>())
}

/// Basis-point deltas of a Fynd quote against the settled amount (positive = Fynd better).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Deltas {
    /// `fynd amount_out` vs the settled amount, both gross of gas — always like-for-like, and
    /// the basis of the headline [`Verdict`].
    pub raw_bps: Option<f64>,
    /// Secondary, recorded for later gas analysis: `fynd amount_out_net_gas` vs the settled
    /// amount net of the gas the trader paid for it. Asymmetric when the settled gas is unknown
    /// (the settled side stays gross while Fynd's is charged) — most Relay settlements are
    /// operator-submitted so their trader gas is legitimately absent. Not used for verdicts.
    pub net_bps: Option<f64>,
}

impl Deltas {
    const NONE: Self = Self { raw_bps: None, net_bps: None };
}

/// Win/loss classification for a single trade, judged at one block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    /// Fynd's gross output strictly beats the gross settled output ([`Deltas::raw_bps`] > 0).
    Win,
    /// Fynd's output was equal to or worse than settled, or it could not be compared.
    Loss,
    /// Fynd only returned a partial route for the trade's size — a coverage miss, not a fair loss.
    CoverageMiss,
    /// Fynd could not solve the trade at all.
    Unsolvable,
    /// The trade's settlement was bracketed by a front-run and a back-run (see
    /// `decoder::sandwich`): the settled output was moved by MEV, not by inferior routing, so
    /// this is not a fair win or loss. The bps/USD deltas are still computed and written to
    /// JSONL for offline analysis; only the classification changes.
    Sandwiched,
}

/// Minimum fraction of the settled output Fynd must produce for the result to count as a real
/// comparison. Below this, Fynd could not serve the trade's size (thin liquidity made it return
/// a route covering only part of it), so the result is a coverage miss rather than a near-total
/// loss. Without this cut, a single un-fillable whale trade dominates the USD aggregate.
const MIN_FILL_RATIO: f64 = 0.5;

/// Reclassify a Fynd quote that covers far less than the settled amount as a coverage miss.
///
/// Fynd does not do price-constrained partial fills, but when liquidity is thin it can return a
/// route for only part of the requested size. Both amounts are in `token_out` units, so their
/// ratio is unit-free. A `Solved` outcome whose raw output is below [`MIN_FILL_RATIO`] of the
/// settled amount becomes [`Outcome::Partial`]; every other outcome passes through unchanged.
pub(crate) fn served(outcome: Outcome, settled_amount_out: U256) -> Outcome {
    let Outcome::Solved(ref solved) = outcome else {
        return outcome;
    };
    let settled = usd::u256_to_f64(settled_amount_out);
    let fynd = usd::u256_to_f64(solved.amount_out);
    if settled > 0.0 && fynd < MIN_FILL_RATIO * settled {
        return Outcome::Partial(format!(
            "partial route: {:.0}% of settled size",
            fynd / settled * 100.0
        ));
    }
    outcome
}

/// Compute raw and net-of-gas bps deltas of `outcome` against the settled trade.
///
/// `raw_bps` compares gross outputs; `net_bps` compares both sides net of their own gas —
/// `settled_net_gas` is the settled output minus the gas the trader paid for the route, and
/// equals `settled_amount_out` when that gas is unknown or was paid by someone else.
pub(crate) fn compare(
    outcome: &Outcome,
    settled_amount_out: U256,
    settled_net_gas: U256,
) -> Deltas {
    let Outcome::Solved(solved) = outcome else {
        return Deltas::NONE;
    };
    Deltas {
        raw_bps: raw_bps_diff(&to_biguint(solved.amount_out), &to_biguint(settled_amount_out)),
        net_bps: raw_bps_diff(&to_biguint(solved.amount_out_net_gas), &to_biguint(settled_net_gas)),
    }
}

/// Classify a trade by its gross output delta: Fynd wins only when it delivers strictly more
/// output than the trade settled for, both sides gross of gas.
///
/// Gross-vs-gross is the one comparison that is always like-for-like: the settled route's gas is
/// often legitimately unattributable (most Relay settlements are submitted by Relay's own
/// operators, so the trader paid no gas), and a net-vs-gross fallback would mix comparison bases
/// across records. The net numbers are still recorded ([`Deltas::net_bps`]) for later analysis.
pub(crate) fn verdict(outcome: &Outcome, deltas: &Deltas) -> Verdict {
    if let Outcome::Partial(_) = outcome {
        return Verdict::CoverageMiss;
    }
    match deltas.raw_bps {
        Some(d) if d > 0.0 => Verdict::Win,
        Some(_) => Verdict::Loss,
        None => Verdict::Unsolvable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::SolvedAmount;

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            quote_json: None,
        })
    }

    /// Settled side without a known gas cost: net compares against the gross settled amount.
    fn gross(settled: u64) -> (U256, U256) {
        (U256::from(settled), U256::from(settled))
    }

    #[test]
    fn test_compare_fynd_better() {
        let (settled, net) = gross(10_000);
        let d = compare(&solved(10_100, 10_050), settled, net);
        assert!((d.raw_bps.unwrap() - 100.0).abs() < 0.01);
        assert!((d.net_bps.unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_compare_fynd_worse() {
        let (settled, net) = gross(10_000);
        let d = compare(&solved(9_900, 9_800), settled, net);
        assert!(d.raw_bps.unwrap() < 0.0);
        assert!(d.net_bps.unwrap() < 0.0);
    }

    #[test]
    fn test_compare_with_settled_gas() {
        // Settled 10_000 gross but its trader paid 100 in gas: raw still compares gross vs
        // gross; net compares 10_050 vs 9_900.
        let d = compare(&solved(10_100, 10_050), U256::from(10_000u64), U256::from(9_900u64));
        assert!((d.raw_bps.unwrap() - 100.0).abs() < 0.01);
        assert!((d.net_bps.unwrap() - 151.5).abs() < 0.1);
    }

    #[test]
    fn test_compare_unsolvable() {
        let (settled, net) = gross(10_000);
        let d = compare(&Outcome::Unsolvable("no route".into()), settled, net);
        assert_eq!(d, Deltas::NONE);
    }

    #[test]
    fn test_compare_zero_settled() {
        let d = compare(&solved(10_000, 10_000), U256::ZERO, U256::ZERO);
        assert_eq!(d.raw_bps, None);
    }

    #[test]
    fn test_verdict_win_threshold() {
        let (settled, net) = gross(10_000);
        let outcome = solved(10_100, 10_050);
        assert_eq!(verdict(&outcome, &compare(&outcome, settled, net)), Verdict::Win);
    }

    #[test]
    fn test_verdict_gross_better_net_worse() {
        // Gross output wins even though Fynd's own gas would eat the edge: the headline verdict
        // compares gross vs gross, and the net delta stays available as a secondary number.
        let (settled, net) = gross(10_000);
        let outcome = solved(10_100, 9_990);
        assert_eq!(verdict(&outcome, &compare(&outcome, settled, net)), Verdict::Win);
    }

    #[test]
    fn test_verdict_with_settled_gas() {
        // The settled trader's gas does not move the verdict in either direction — only the
        // gross outputs do.
        let fynd = solved(10_050, 9_990);
        let settled = U256::from(10_000u64);
        assert_eq!(verdict(&fynd, &compare(&fynd, settled, settled)), Verdict::Win);
        assert_eq!(verdict(&fynd, &compare(&fynd, settled, U256::from(9_900u64))), Verdict::Win);
        let worse = solved(9_900, 9_800);
        assert_eq!(verdict(&worse, &compare(&worse, settled, U256::from(9_000u64))), Verdict::Loss);
    }

    #[test]
    fn test_verdict_unsolvable() {
        let (settled, net) = gross(10_000);
        let outcome = Outcome::Unsolvable("missing token".into());
        assert_eq!(verdict(&outcome, &compare(&outcome, settled, net)), Verdict::Unsolvable);
    }

    #[test]
    fn test_served_partial_route() {
        // Fynd covered only 40% of the settled size → coverage miss, not a loss.
        let outcome = served(solved(400, 390), U256::from(1_000u64));
        assert!(matches!(outcome, Outcome::Partial(_)));
        let (settled, net) = gross(1_000);
        assert_eq!(verdict(&outcome, &compare(&outcome, settled, net)), Verdict::CoverageMiss);
    }

    #[test]
    fn test_served_adequate_fill() {
        // 90% coverage is a real (worse) quote, not a coverage miss; the floor is kept.
        assert!(matches!(served(solved(900, 880), U256::from(1_000u64)), Outcome::Solved(_)));
        assert!(matches!(served(solved(500, 490), U256::from(1_000u64)), Outcome::Solved(_)));
    }

    #[test]
    fn test_served_unsolvable_and_zero_settled() {
        assert!(matches!(
            served(Outcome::Unsolvable("x".into()), U256::from(1_000u64)),
            Outcome::Unsolvable(_)
        ));
        assert!(matches!(served(solved(1, 1), U256::ZERO), Outcome::Solved(_)));
    }
}
