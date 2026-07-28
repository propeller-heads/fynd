//! Collects what the mock `PropAMM` pool won, block by block.
//!
//! The mock quotes at its configured fee-free price and charges nothing, so when the router selects
//! it, everything the route produces above the public commitment is **fee headroom**: the fee the
//! signed extension could have charged and still won the trade. The router already computes exactly
//! that split — [`OrderQuote::committed_amount_out`] is what the user is promised and
//! [`OrderQuote::surplus_amount`] is the excess — so this module only has to read it, express it in
//! bps, and total it up.
//!
//! Because the quoted `amount_out` is pinned to the commitment, the mock pool never changes
//! hindsight's own win/loss verdict. It only adds this second, orthogonal measurement.

use std::sync::Mutex;

use alloy::primitives::Address;
use fynd_core::OrderQuote;
use num_bigint::BigUint;
use serde::Serialize;

use crate::propamm::MOCK_COMPONENT_ID;

/// One re-solved order's mock-`PropAMM` outcome.
#[derive(Debug, Clone)]
pub(crate) struct Observation {
    /// Order output token — the token both amounts below are denominated in, and the one their USD
    /// valuation is taken against. The order's input token and size are not repeated here: the
    /// comparison record this joins onto already carries them.
    pub token_out: Address,
    /// Whether the solver produced a successful quote at all. Unsuccessful solves are still
    /// recorded so the sink stays index-aligned with the block's trades, which is how each
    /// observation is joined back to its comparison record.
    pub solved: bool,
    /// Whether the winning route ran through the mock `PropAMM` pool.
    pub won: bool,
    /// The public-market output the user is committed to. `None` when the mock pool didn't win.
    pub committed_amount_out: Option<BigUint>,
    /// Output the mock produced above the commitment — the fee it could have charged. `None` when
    /// it didn't win.
    pub fee_headroom: Option<BigUint>,
}

impl Observation {
    /// Reads a re-solved quote's mock-`PropAMM` outcome.
    ///
    /// A win is a winning route that contains the mock component — not merely a non-empty surplus,
    /// which can legitimately round to zero on a marginal beat.
    pub(crate) fn from_quote(quote: &OrderQuote, token_out: Address) -> Self {
        let solved = quote.status() == fynd_core::types::QuoteStatus::Success;
        let won = solved &&
            quote.route().is_some_and(|route| {
                route
                    .swaps()
                    .iter()
                    .any(|swap| swap.component_id() == MOCK_COMPONENT_ID)
            });
        Self {
            token_out,
            solved,
            won,
            committed_amount_out: quote.committed_amount_out().cloned(),
            fee_headroom: quote.surplus_amount().cloned(),
        }
    }

    /// The fee the mock could have charged, as a fraction of the committed output, in basis points.
    ///
    /// This is the headline per-trade number: "the pool could have taken this much fee and the user
    /// would still have been better off than on the public market." `None` when the pool didn't win
    /// or the commitment is zero.
    pub(crate) fn fee_headroom_bps(&self) -> Option<f64> {
        let committed = biguint_to_f64(self.committed_amount_out.as_ref()?);
        let headroom = biguint_to_f64(self.fee_headroom.as_ref()?);
        if committed <= 0.0 {
            return None;
        }
        Some(headroom / committed * 10_000.0)
    }
}

/// Running totals over a whole monitor run.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Totals {
    /// Orders re-solved into a successful quote.
    pub solved: u64,
    /// Of those, how many routed through the mock `PropAMM` pool.
    pub won: u64,
    /// Fee headroom valued in USD at top-of-block prices.
    pub headroom_usd: f64,
    /// Committed output valued in USD — the flow the pool captured.
    pub captured_flow_usd: f64,
}

impl Totals {
    /// Share of solved orders the mock pool won, as a percentage. Zero when nothing solved.
    // Precision loss is irrelevant: these are trade counts, far below f64's exact-integer range.
    #[expect(clippy::cast_precision_loss)]
    pub(crate) fn winrate_pct(&self) -> f64 {
        if self.solved == 0 {
            return 0.0;
        }
        self.won as f64 / self.solved as f64 * 100.0
    }

    /// Fee headroom as a fraction of captured flow, in basis points — the average fee the pool
    /// could have charged across everything it won. Zero when it captured no flow.
    pub(crate) fn avg_fee_headroom_bps(&self) -> f64 {
        if self.captured_flow_usd <= 0.0 {
            return 0.0;
        }
        self.headroom_usd / self.captured_flow_usd * 10_000.0
    }
}

/// Shared sink the solve path writes observations into and the block loop drains.
///
/// `StepAdapter::solve` takes `&self`, so the sink owns its mutability. A `Mutex` is right here:
/// contention is one lock per re-solved order.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    pending: Mutex<Vec<Observation>>,
    totals: Mutex<Totals>,
    /// The mirrored pair as token symbols, e.g. `WETH/USDC`. Resolved once the feed has loaded the
    /// tokens, which is why it is set from the injection path rather than from the CLI.
    pair_label: Mutex<Option<String>>,
}

impl Stats {
    /// Records one re-solved order.
    pub(crate) fn record(&self, observation: Observation) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.push(observation);
        }
    }

    /// Takes everything recorded since the last drain.
    pub(crate) fn drain(&self) -> Vec<Observation> {
        self.pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }

    /// Folds a block's observations into the run totals and returns the updated snapshot.
    pub(crate) fn accumulate(
        &self,
        observations: &[Observation],
        headroom_usd: f64,
        captured_flow_usd: f64,
    ) -> Totals {
        let Ok(mut totals) = self.totals.lock() else {
            return Totals::default();
        };
        totals.solved += observations
            .iter()
            .filter(|o| o.solved)
            .count() as u64;
        totals.won += observations
            .iter()
            .filter(|o| o.won)
            .count() as u64;
        totals.headroom_usd += headroom_usd;
        totals.captured_flow_usd += captured_flow_usd;
        *totals
    }

    /// Records the mirrored pair's symbol label, so the report can name it.
    pub(crate) fn set_pair_label(&self, label: &str) {
        if let Ok(mut pair_label) = self.pair_label.lock() {
            if pair_label.is_none() {
                *pair_label = Some(label.to_string());
            }
        }
    }

    /// The mirrored pair's symbol label, once an injection has resolved it.
    pub(crate) fn pair_label(&self) -> Option<String> {
        self.pair_label
            .lock()
            .ok()
            .and_then(|label| label.clone())
    }

    /// The run totals so far.
    pub(crate) fn totals(&self) -> Totals {
        self.totals
            .lock()
            .map(|totals| *totals)
            .unwrap_or_default()
    }
}

/// Converts a `BigUint` to `f64` for ratio reporting. Saturates to infinity beyond `f64` range,
/// which the callers guard against by dividing only by positive finite values.
pub(crate) fn biguint_to_f64(value: &BigUint) -> f64 {
    value
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::INFINITY)
}

/// The mock-`PropAMM` fields written into a comparison record, so the offline `report` subcommand
/// reads them alongside everything else it already knows about the trade (venue, solver, pair).
///
/// Amounts are decimal strings, like every other amount in the record: they exceed `f64`'s exact
/// integer range and JSON has no integer type wide enough.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Record {
    /// The mirrored pair as token symbols, e.g. `WETH/USDC` — which pool the mock stood in for.
    pub pair: Option<String>,
    /// Whether the winning route ran through the mock `PropAMM` pool.
    pub won: bool,
    /// The public-market output the user is committed to.
    pub committed_amount_out: Option<String>,
    /// Output the mock produced above the commitment — the fee it could have charged.
    pub fee_headroom: Option<String>,
    /// That headroom as a fraction of the commitment, in basis points.
    pub fee_headroom_bps: Option<f64>,
    /// The committed output valued in USD — the flow the pool captured.
    pub committed_usd: Option<f64>,
    /// The headroom valued in USD.
    pub fee_headroom_usd: Option<f64>,
}

impl Record {
    /// Projects an observation into its record, given the USD valuations the caller computed.
    pub(crate) fn new(
        observed: &Observation,
        pair: Option<String>,
        committed_usd: Option<f64>,
        fee_headroom_usd: Option<f64>,
    ) -> Self {
        Self {
            pair,
            won: observed.won,
            committed_amount_out: observed
                .committed_amount_out
                .as_ref()
                .map(std::string::ToString::to_string),
            fee_headroom: observed
                .fee_headroom
                .as_ref()
                .map(std::string::ToString::to_string),
            fee_headroom_bps: observed.fee_headroom_bps(),
            committed_usd,
            fee_headroom_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(won: bool, committed: u64, headroom: u64) -> Observation {
        Observation {
            token_out: Address::from([0x22; 20]),
            solved: true,
            won,
            committed_amount_out: won.then(|| BigUint::from(committed)),
            fee_headroom: won.then(|| BigUint::from(headroom)),
        }
    }

    #[test]
    fn test_fee_headroom_bps_from_committed_and_headroom() {
        let observed = observation(true, 1_000_000, 500);
        assert!(
            (observed
                .fee_headroom_bps()
                .expect("a win reports headroom") -
                5.0)
            .abs() <
                1e-9
        );
    }

    #[test]
    fn test_fee_headroom_bps_absent_when_pool_lost() {
        assert!(observation(false, 0, 0)
            .fee_headroom_bps()
            .is_none());
    }

    #[test]
    fn test_fee_headroom_bps_absent_on_zero_commitment() {
        // A zero commitment would divide by zero; the ratio is undefined, not infinite.
        let mut observed = observation(true, 0, 500);
        observed.committed_amount_out = Some(BigUint::ZERO);
        assert!(observed.fee_headroom_bps().is_none());
    }

    #[test]
    fn test_accumulate_sums_across_blocks() {
        let stats = Stats::default();
        stats.accumulate(&[observation(true, 100, 1), observation(false, 0, 0)], 2.0, 400.0);
        let totals = stats.accumulate(&[observation(true, 100, 1)], 1.0, 200.0);

        assert_eq!(totals.solved, 3);
        assert_eq!(totals.won, 2);
        assert!((totals.headroom_usd - 3.0).abs() < 1e-9);
        assert!((totals.captured_flow_usd - 600.0).abs() < 1e-9);
        assert!((totals.winrate_pct() - 200.0 / 3.0).abs() < 1e-9);
        // 3 USD of headroom on 600 USD of captured flow = 50 bps.
        assert!((totals.avg_fee_headroom_bps() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn test_empty_totals_report_zero_rather_than_nan() {
        let totals = Totals::default();
        assert!(totals.winrate_pct().abs() < f64::EPSILON);
        assert!(totals.avg_fee_headroom_bps().abs() < f64::EPSILON);
    }

    #[test]
    fn test_drain_empties_the_sink() {
        let stats = Stats::default();
        stats.record(observation(true, 100, 1));
        stats.record(observation(false, 0, 0));

        assert_eq!(stats.drain().len(), 2);
        assert!(stats.drain().is_empty(), "a second drain must not repeat observations");
    }
}
