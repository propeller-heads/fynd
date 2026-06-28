//! Pure comparison math between a Fynd re-solve and the on-chain settled amount.

use alloy::primitives::U256;
use fynd_tools_common::bps::raw_bps_diff;
use serde::Serialize;

use crate::resolve::Outcome;

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

/// Win/loss/unsolvable classification for a single trade, judged at one block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    /// Fynd would have produced strictly more output than settled (net of gas).
    Win,
    /// Fynd's output was equal to or worse than settled, or it could not be compared.
    Loss,
    /// Fynd could not solve the trade at all.
    Unsolvable,
}

/// Compute raw and net-of-gas bps deltas of `outcome` against `settled_amount_out`.
pub(crate) fn compare(outcome: &Outcome, settled_amount_out: U256) -> Deltas {
    let Outcome::Solved(solved) = outcome else {
        return Deltas::NONE;
    };
    let settled = settled_amount_out.to_string();
    Deltas {
        raw_bps: raw_bps_diff(&solved.amount_out.to_string(), &settled),
        net_bps: raw_bps_diff(&solved.amount_out_net_gas.to_string(), &settled),
    }
}

/// Classify a trade by its net-of-gas delta at the given outcome (top-of-block is the default
/// reference). Fynd wins only when it delivers strictly more output after gas.
pub(crate) fn verdict(outcome: &Outcome, settled_amount_out: U256) -> Verdict {
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
}
