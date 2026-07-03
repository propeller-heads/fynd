//! Pure comparison math between a Fynd re-solve and the on-chain settled amount.

use alloy::primitives::U256;
use fynd_tools_common::bps::raw_bps_diff;
use num_bigint::BigUint;
use serde::Serialize;

use crate::resolve::Outcome;

/// Convert an alloy `U256` token amount to `BigUint` for the bps helpers, big-endian and without a
/// decimal string round-trip.
fn to_biguint(amount: U256) -> BigUint {
    BigUint::from_bytes_be(&amount.to_be_bytes::<32>())
}

/// Basis-point deltas of a Fynd quote against the settled amount (positive = Fynd better).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Deltas {
    /// `fynd amount_out` vs settled, ignoring gas.
    pub raw_bps: Option<f64>,
    /// `fynd amount_out_net_gas` vs settled — Fynd's output after its own gas cost.
    pub net_bps: Option<f64>,
}

impl Deltas {
    const NONE: Self = Self { raw_bps: None, net_bps: None };
}

/// Win/loss classification for a single trade, judged at one block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    /// Fynd delivered strictly more output than settled after subtracting only its own gas (see
    /// [`verdict`] for why this comparison is intentionally conservative).
    Win,
    /// Fynd's output was equal to or worse than settled, or it could not be compared.
    Loss,
    /// Fynd only returned a partial route for the trade's size — a coverage miss, not a fair loss.
    CoverageMiss,
    /// Fynd could not solve the trade at all.
    Unsolvable,
}

/// Minimum fraction of the settled output Fynd must produce for the result to count as a real
/// comparison. Below this, Fynd could not serve the trade's size — because liquidity was too thin
/// it returned a route covering only part of it — so the result is a coverage miss rather than a
/// near-total loss; otherwise a single un-fillable whale dominates the USD aggregate.
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
    let settled: f64 = settled_amount_out
        .to_string()
        .parse()
        .unwrap_or(0.0);
    let fynd: f64 = solved
        .amount_out
        .to_string()
        .parse()
        .unwrap_or(0.0);
    if settled > 0.0 && fynd < MIN_FILL_RATIO * settled {
        return Outcome::Partial(format!(
            "partial route: {:.0}% of settled size",
            fynd / settled * 100.0
        ));
    }
    outcome
}

/// Compute raw and net-of-gas bps deltas of `outcome` against `settled_amount_out`.
pub(crate) fn compare(outcome: &Outcome, settled_amount_out: U256) -> Deltas {
    let Outcome::Solved(solved) = outcome else {
        return Deltas::NONE;
    };
    let settled = to_biguint(settled_amount_out);
    Deltas {
        raw_bps: raw_bps_diff(&to_biguint(solved.amount_out), &settled),
        net_bps: raw_bps_diff(&to_biguint(solved.amount_out_net_gas), &settled),
    }
}

/// Classify a trade by its net-of-gas delta against the settled amount. Fynd wins only when it
/// delivers strictly more output after subtracting its own estimated gas cost.
///
/// The comparison is deliberately conservative and asymmetric: Fynd's output is net of its own
/// gas, while the settled `amount_out` is gross — the settled swap's gas was paid separately in ETH
/// and is not subtracted here. Isolating the on-chain trade's true gas cost is hard (it can sit
/// deep in a larger transaction alongside unrelated activity), so a `Win` under-counts rather than
/// over-counts Fynd's edge. Symmetric N-1/N gas accounting is a follow-up.
pub(crate) fn verdict(outcome: &Outcome, settled_amount_out: U256) -> Verdict {
    if let Outcome::Partial(_) = outcome {
        return Verdict::CoverageMiss;
    }
    match compare(outcome, settled_amount_out).net_bps {
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

    #[test]
    fn compare_fynd_better_is_positive() {
        let d = compare(&solved(10_100, 10_050), U256::from(10_000u64));
        assert!((d.raw_bps.unwrap() - 100.0).abs() < 0.01);
        assert!((d.net_bps.unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn compare_fynd_worse_is_negative() {
        let d = compare(&solved(9_900, 9_800), U256::from(10_000u64));
        assert!(d.raw_bps.unwrap() < 0.0);
        assert!(d.net_bps.unwrap() < 0.0);
    }

    #[test]
    fn compare_unsolvable_is_none() {
        let d = compare(&Outcome::Unsolvable("no route".into()), U256::from(10_000u64));
        assert_eq!(d, Deltas::NONE);
    }

    #[test]
    fn compare_zero_settled_is_none() {
        let d = compare(&solved(10_000, 10_000), U256::ZERO);
        assert_eq!(d.raw_bps, None);
    }

    #[test]
    fn verdict_win_only_when_net_positive() {
        assert_eq!(verdict(&solved(10_100, 10_050), U256::from(10_000u64)), Verdict::Win);
    }

    #[test]
    fn verdict_loss_when_net_not_better() {
        // Raw is better but net-of-gas is worse → loss (gas ate the edge).
        assert_eq!(verdict(&solved(10_100, 9_990), U256::from(10_000u64)), Verdict::Loss);
    }

    #[test]
    fn verdict_unsolvable_passthrough() {
        assert_eq!(
            verdict(&Outcome::Unsolvable("missing token".into()), U256::from(10_000u64)),
            Verdict::Unsolvable
        );
    }

    #[test]
    fn served_reclassifies_partial_route_as_partial() {
        // Fynd covered only 40% of the settled size → coverage miss, not a loss.
        let outcome = served(solved(400, 390), U256::from(1_000u64));
        assert!(matches!(outcome, Outcome::Partial(_)));
        assert_eq!(verdict(&outcome, U256::from(1_000u64)), Verdict::CoverageMiss);
    }

    #[test]
    fn served_keeps_adequate_fill() {
        // 90% coverage is a real (worse) quote, not a coverage miss; the floor is kept.
        assert!(matches!(served(solved(900, 880), U256::from(1_000u64)), Outcome::Solved(_)));
        assert!(matches!(served(solved(500, 490), U256::from(1_000u64)), Outcome::Solved(_)));
    }

    #[test]
    fn served_passes_through_unsolvable_and_zero_settled() {
        assert!(matches!(
            served(Outcome::Unsolvable("x".into()), U256::from(1_000u64)),
            Outcome::Unsolvable(_)
        ));
        assert!(matches!(served(solved(1, 1), U256::ZERO), Outcome::Solved(_)));
    }
}
