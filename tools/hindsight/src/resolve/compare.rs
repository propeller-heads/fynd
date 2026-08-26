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

/// Basis-point delta of a Fynd quote against the settled amount (positive = Fynd better).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Deltas {
    /// `fynd amount_out` vs the settled amount, both gross of gas — always like-for-like, and
    /// the basis of the headline `Verdict`.
    pub raw_bps: Option<f64>,
}

impl Deltas {
    const NONE: Self = Self { raw_bps: None };
}

/// Slippage of the top-of-block route re-executed at back-of-block: how the route's output moved
/// between quote time (N-1) and execution time (N). Positive = the route produced more than
/// quoted — the surplus we would keep if we charged positive slippage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Slippage {
    /// Re-executed output vs the quoted output, in bps (positive = surplus).
    pub bps: f64,
    /// The top-of-block quoted output the slippage is measured against.
    pub quoted_amount_out: U256,
    /// The same route's re-executed output at back-of-block.
    pub reexecuted_amount_out: U256,
}

/// Slippage between the top-of-block quote and its re-execution at back-of-block. `None` when
/// either side is unsolved (no route quoted, or the re-execution failed) or the quoted output
/// is zero.
pub(crate) fn slippage(top: &Outcome, back: &Outcome) -> Option<Slippage> {
    let (Outcome::Solved(top), Outcome::Solved(back)) = (top, back) else {
        return None;
    };
    let bps = raw_bps_diff(&to_biguint(back.amount_out), &to_biguint(top.amount_out))?;
    Some(Slippage {
        bps,
        quoted_amount_out: top.amount_out,
        reexecuted_amount_out: back.amount_out,
    })
}

/// Win/loss classification for a single trade, judged at one block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    /// Fynd's gross output strictly beats the gross settled output (`Deltas::raw_bps` > 0).
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
    /// Fynd's output is more than `MAX_WIN_RATIO` times the settled output, which no routing edge
    /// produces: the settled amount this is measured against is not the trade's real output. Like
    /// `Sandwiched`, the deltas are still written to JSONL so the record stays studyable, and the
    /// classification keeps it out of the aggregates.
    ImplausibleSettledAmount,
}

/// The most output Fynd can produce against a settled trade before the settled amount itself is
/// the thing that must be wrong.
///
/// A real routing edge is a few percent, occasionally tens of percent on a thin pair. Past a small
/// multiple the comparison is not measuring routing: netting has paired the trade's input with a
/// dust receipt, so the settled output belongs to a different trade or to no trade. Seen live on
/// Ethereum: 3,555.90 USDC in, paired with 0.00000063 ETH out, reported as a $3,555.81 win because
/// Fynd correctly quoted 1.44 ETH.
///
/// The ceiling was first set at 100x from one staging day (2,527 solved-and-settled Ethereum
/// records), where the largest genuine win was 2.66x and the smallest broken one 1,472,381x. A
/// full production day across Ethereum and Base then landed 103 wins in the gap, every one of them
/// claiming more than the trade's whole notional and none above 99.3x — so the broken records are
/// not two orders of magnitude out, and 100x let all of them through.
///
/// 3x is where the two populations separate in that day: of 55,826 scored wins, every win above 2x
/// claimed more than the trade's notional, so nothing above the ceiling is a win worth keeping,
/// and the largest genuine win yet measured (2.66x) stays under it.
const MAX_WIN_RATIO: f64 = 3.0;

/// Whether Fynd's output is too far above the settled output for the settled amount to be real.
///
/// Both amounts are in `token_out` units, so their ratio is unit-free and needs no prices. Only a
/// solved outcome can be judged, and a settled amount of zero is left to the coverage buckets —
/// there is no ratio to take.
pub(crate) fn implausible_settled_amount(outcome: &Outcome, settled_amount_out: U256) -> bool {
    let Outcome::Solved(solved) = outcome else {
        return false;
    };
    let settled = usd::u256_to_f64(settled_amount_out);
    let fynd = usd::u256_to_f64(solved.amount_out);
    settled > 0.0 && fynd > MAX_WIN_RATIO * settled
}

/// Minimum fraction of the settled output Fynd must produce for the result to count as a real
/// comparison. Below this, Fynd could not serve the trade's size (thin liquidity made it return
/// a route covering only part of it), so the result is a coverage miss rather than a near-total
/// loss. Without this cut, a single un-fillable whale trade dominates the USD aggregate.
const MIN_FILL_RATIO: f64 = 0.5;

/// Settled USD notional below which a trade is left out of the unweighted views: the win rate and
/// the bps quantiles. USD-weighted views keep every trade, as does a trade too small to price.
///
/// Below a dollar a few wei of rounding is thousands of bps, which carries an unweighted median.
/// Above it the spread is real — small trades are genuinely easier to beat — so the floor sits
/// just past the rounding noise rather than where the win rate stops moving.
pub(crate) const MIN_NOTIONAL_USD: f64 = 10.0;

/// Reclassify a Fynd quote that covers far less than the settled amount as a coverage miss.
///
/// Fynd does not do price-constrained partial fills, but when liquidity is thin it can return a
/// route for only part of the requested size. Both amounts are in `token_out` units, so their
/// ratio is unit-free. A `Solved` outcome whose raw output is below `MIN_FILL_RATIO` of the
/// settled amount becomes `Outcome::Partial`; every other outcome passes through unchanged.
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

/// Compute the gross bps delta of `outcome` against the settled trade.
pub(crate) fn compare(outcome: &Outcome, settled_amount_out: U256) -> Deltas {
    let Outcome::Solved(solved) = outcome else {
        return Deltas::NONE;
    };
    Deltas {
        raw_bps: raw_bps_diff(&to_biguint(solved.amount_out), &to_biguint(settled_amount_out)),
    }
}

/// Classify a trade by its gross output delta: Fynd wins only when it delivers strictly more
/// output than the trade settled for, both sides gross of gas.
///
/// Gross-vs-gross is the one comparison that is always like-for-like: the settled route's gas is
/// often legitimately unattributable (most Relay settlements are submitted by Relay's own
/// operators, so the trader paid no gas), and a net-vs-gross fallback would mix comparison bases
/// across records.
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
    #[test]
    fn test_implausible_settled_amount_dust_pairing() {
        // Live staging record 0x24edb6ad…: 3,555.90 USDC in, which netting paired with a
        // 0.00000063 ETH receipt. Fynd correctly quoted 1.44 ETH, so the record claimed a
        // $3,555.81 win — 2.29 billion times the settled output.
        let outcome = solved(1_443_961_056_940_373_096, 1_443_961_056_940_373_096);
        assert!(implausible_settled_amount(&outcome, U256::from(630_533_677u64)));
    }

    #[test]
    fn test_largest_real_win_is_not_implausible() {
        // The largest genuine win yet measured is 2.66x, and it has to stay under the ceiling.
        let outcome = solved(266, 266);
        assert!(!implausible_settled_amount(&outcome, U256::from(100u64)));
    }

    #[test]
    fn test_ratio_either_side_of_the_ceiling() {
        // 3x is allowed; past it the settled amount is the thing that must be wrong.
        assert!(!implausible_settled_amount(&solved(300, 300), U256::from(100u64)));
        assert!(implausible_settled_amount(&solved(301, 301), U256::from(100u64)));
    }

    #[test]
    fn test_win_that_exceeds_the_trades_notional() {
        // The shape the 100x ceiling let through: a production record whose settled side was
        // 13.43 USDC and whose quote was 44.7x that, claiming $587 of savings on a $13 trade.
        let outcome = solved(600_310_000, 600_310_000);
        assert!(implausible_settled_amount(&outcome, U256::from(13_430_000u64)));
    }

    #[test]
    fn test_zero_settled_and_unsolved_are_left_alone() {
        // No ratio to take: a zero settled amount and an unsolved outcome belong to the coverage
        // buckets, not here.
        assert!(!implausible_settled_amount(&solved(1_000, 1_000), U256::ZERO));
        assert!(!implausible_settled_amount(
            &Outcome::Unsolvable("no route".to_string()),
            U256::from(1u64)
        ));
    }

    use super::*;
    use crate::resolve::SolvedAmount;

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            algorithm: String::new(),
            quote_json: None,
            solved_route: None,
        })
    }

    #[test]
    fn test_compare_fynd_better() {
        let d = compare(&solved(10_100, 10_050), U256::from(10_000u64));
        assert!((d.raw_bps.unwrap() - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_compare_fynd_worse() {
        let d = compare(&solved(9_900, 9_800), U256::from(10_000u64));
        assert!(d.raw_bps.unwrap() < 0.0);
    }

    #[test]
    fn test_compare_unsolvable() {
        let d = compare(&Outcome::Unsolvable("no route".into()), U256::from(10_000u64));
        assert_eq!(d, Deltas::NONE);
    }

    #[test]
    fn test_compare_zero_settled() {
        let d = compare(&solved(10_000, 10_000), U256::ZERO);
        assert_eq!(d.raw_bps, None);
    }

    #[test]
    fn test_verdict_win_threshold() {
        let settled = U256::from(10_000u64);
        let outcome = solved(10_100, 10_050);
        assert_eq!(verdict(&outcome, &compare(&outcome, settled)), Verdict::Win);
    }

    #[test]
    fn test_verdict_gross_better_net_worse() {
        // Gross output wins even when Fynd's own gas would eat the edge: the verdict compares
        // gross vs gross.
        let settled = U256::from(10_000u64);
        let outcome = solved(10_100, 9_990);
        assert_eq!(verdict(&outcome, &compare(&outcome, settled)), Verdict::Win);
    }

    #[test]
    fn test_verdict_unsolvable() {
        let outcome = Outcome::Unsolvable("missing token".into());
        assert_eq!(
            verdict(&outcome, &compare(&outcome, U256::from(10_000u64))),
            Verdict::Unsolvable
        );
    }

    #[test]
    fn test_served_partial_route() {
        // Fynd covered only 40% of the settled size → coverage miss, not a loss.
        let outcome = served(solved(400, 390), U256::from(1_000u64));
        assert!(matches!(outcome, Outcome::Partial(_)));
        assert_eq!(
            verdict(&outcome, &compare(&outcome, U256::from(1_000u64))),
            Verdict::CoverageMiss
        );
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

    #[test]
    fn slippage_positive_when_reexecution_beats_the_quote() {
        // Quoted 10_000 at top, re-executed to 10_050 at back → +50 bps surplus.
        let s = slippage(&solved(10_000, 9_900), &solved(10_050, 9_950)).unwrap();
        assert!((s.bps - 50.0).abs() < 0.01, "expected +50 bps, got {}", s.bps);
        assert_eq!(s.quoted_amount_out, U256::from(10_000u64));
        assert_eq!(s.reexecuted_amount_out, U256::from(10_050u64));
    }

    #[test]
    fn slippage_negative_when_reexecution_underperforms() {
        let s = slippage(&solved(10_000, 9_900), &solved(9_900, 9_800)).unwrap();
        assert!((s.bps + 100.0).abs() < 0.01, "expected -100 bps, got {}", s.bps);
    }

    #[test]
    fn slippage_none_when_either_side_unsolved() {
        let failed = Outcome::Unsolvable("re-execution failed".into());
        assert_eq!(slippage(&failed, &solved(10_000, 9_900)), None);
        assert_eq!(slippage(&solved(10_000, 9_900), &failed), None);
    }

    #[test]
    fn slippage_none_for_zero_quoted_output() {
        assert_eq!(slippage(&solved(0, 0), &solved(10, 10)), None);
    }
}
