//! One measurement of a quoted route replayed `offset` blocks later, and its JSONL projection.

use alloy::primitives::{Address, U256};
use fynd_tools_common::bps::raw_bps_diff;
use num_bigint::BigUint;
use serde::Serialize;

use crate::{
    decay::sample::TradeShape,
    resolve::{render_route, SolvedAmount},
};

/// Why a replay produced no measurement. Kept apart from a plain error string because a pool
/// leaving the feed is a distinct, meaningful outcome — the route became unexecutable, which is
/// itself the revert signal we are looking for — rather than noise to be lumped in with a
/// simulation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplayFailure {
    /// A pool in the route no longer has simulation state: it was removed by the feed or filtered
    /// out since the quote. The route could not have executed at this block.
    PoolGone,
    /// The route replayed but a pool's simulation failed, or the replay returned nothing usable.
    SimulationFailed,
    /// The fresh solve at this block found no route, so market movement has no reference and the
    /// decomposition cannot be computed.
    NoMarketReference,
    /// The replay and the fresh solve both succeeded, but the quoted, replayed, or fresh amount
    /// was zero, so [`DecayBps::new`] could not form a ratio. A zero replayed amount is itself the
    /// extreme decay case — the held route became worthless — so this is marked as a failure
    /// rather than silently dropped, which would make the reported tail optimistic.
    ZeroAmount,
}

impl ReplayFailure {
    /// Classify a `reexecute` failure string. `fynd_core::ReplayError::MissingState` renders as
    /// "no simulation state for component …", which is the one case we separate out.
    pub(crate) fn classify(reason: &str) -> Self {
        if reason.contains("no simulation state for component") {
            Self::PoolGone
        } else {
            Self::SimulationFailed
        }
    }
}

/// The three bps figures for one (trade, offset) measurement.
///
/// Sign convention follows hindsight's [`crate::resolve::Slippage`]: **positive means surplus** —
/// the later state produced more output than the original quote. PR #297 reported the opposite
/// (positive = decay), so flip these before comparing against its Ethereum numbers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct DecayBps {
    /// The quoted route replayed at the later state, against its own original quote. The total
    /// move: market drift plus whatever the route lost by being stale.
    pub route_slippage_bps: f64,
    /// A freshly solved quote at the later state, against the original quote. The part of the move
    /// that any router would have suffered — unavoidable market drift.
    pub market_movement_bps: f64,
    /// `route_slippage_bps - market_movement_bps`, clamped at zero: the part specific to holding
    /// a stale route, which better routing or faster submission could recover. Negative means the
    /// stale route underperformed a fresh solve. Never positive — see
    /// [`Self::execution_slippage_clamped`].
    pub execution_slippage_bps: f64,
    /// Whether the raw `route_slippage_bps - market_movement_bps` was positive — the held route
    /// beating a fresh solve at the same state — before it was clamped to zero. A real in-block
    /// solver holds the original route and takes the better of the two, so its outcome is
    /// `max(replayed, fresh)`; a fresh solve that comes back worse than the route it already found
    /// a few blocks earlier is the solver failing to reproduce its own result, not a genuine
    /// opportunity the route missed. Left uncancelled, these wrong-sign cases average against the
    /// real losses and understate the reported decay. Kept on the record (rather than dropped) so
    /// the solver-inconsistency rate stays visible and offline analysis can still see the raw
    /// signal.
    pub execution_slippage_clamped: bool,
}

impl DecayBps {
    /// Build the decomposition from three amounts in the output token's units.
    ///
    /// Returns `None` when the quoted, replayed, or fresh amount is zero: `raw_bps_diff` treats a
    /// zero on either side of a ratio as undefined, and every one of the three amounts appears as
    /// one side of the two ratios below.
    pub(crate) fn new(quoted: U256, replayed: U256, fresh: U256) -> Option<Self> {
        let route_slippage_bps = bps_against(quoted, replayed)?;
        let market_movement_bps = bps_against(quoted, fresh)?;
        let raw_execution_slippage_bps = route_slippage_bps - market_movement_bps;
        Some(Self {
            route_slippage_bps,
            market_movement_bps,
            execution_slippage_bps: raw_execution_slippage_bps.min(0.0),
            execution_slippage_clamped: raw_execution_slippage_bps > 0.0,
        })
    }
}

/// `later` versus `quoted`, in bps, positive when `later` is larger.
///
/// `raw_bps_diff(baseline, other)` computes `(baseline - other) / other`, so passing the later
/// amount as the baseline and the original quote as `other` yields "how much more the later state
/// produced", matching `resolve::compare::slippage`'s convention.
fn bps_against(quoted: U256, later: U256) -> Option<f64> {
    raw_bps_diff(&to_biguint(later), &to_biguint(quoted))
}

/// Convert an alloy `U256` to `BigUint` big-endian, without a decimal string round-trip.
fn to_biguint(amount: U256) -> BigUint {
    BigUint::from_bytes_be(&amount.to_be_bytes::<32>())
}

/// One measurement: a trade quoted at `quote_block`, replayed at `measured_block`.
///
/// Every record carries either `bps` (a complete measurement) or `failure` (why there is none), so
/// a missing measurement is always attributable rather than silently absent from the output.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DecayRecord {
    /// Groups the offsets measured from one quote, and one sample, together.
    pub round_id: u64,
    /// How many blocks after the quote this measurement was taken, `1..=offsets`.
    pub offset: u32,
    /// The block whose state the route was quoted against.
    pub quote_block: u64,
    /// The block whose state the route was replayed against.
    pub measured_block: u64,
    pub token_in: Address,
    pub token_out: Address,
    /// Decimal string: an exact `U256` does not survive JSON's number type.
    pub amount_in: String,
    /// The original quote's output at `quote_block`.
    pub quoted_amount_out: String,
    /// The same route's output replayed at `measured_block`. Absent when the replay failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replayed_amount_out: Option<String>,
    /// A fresh solve's output at `measured_block`, the market-movement reference. Absent when the
    /// fresh solve found no route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh_amount_out: Option<String>,
    /// The decomposition. Absent when either leg is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bps: Option<DecayBps>,
    /// Why this measurement has no `bps`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<ReplayFailure>,
    /// The algorithm that produced the quote whose route is being tested for staleness — the same
    /// value carried on every offset of a round, since every offset replays the same quote.
    pub quote_algorithm: String,
    /// The fresh solve's route at `measured_block`, rendered as a readable path (see
    /// [`crate::resolve::render_route`]). Absent when the fresh solve found no route.
    ///
    /// Recorded so two situations that `execution_slippage_bps` alone cannot tell apart become
    /// distinguishable: the fresh solve finding a genuinely different, better route (a real,
    /// measurable gain) versus returning the same route with a different number (solver
    /// inconsistency — see [`DecayBps::execution_slippage_clamped`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh_route: Option<String>,
}

/// Everything a round holds about one quoted trade, so each of its offsets can be measured against
/// the same original quote.
///
/// `quote` is kept whole because [`crate::resolve::SteppingSolver::reexecute`] replays
/// `quote.solved_route` — the round holds the route itself for its whole lifetime, which is what
/// makes "the same route, later" measurable at all.
pub(crate) struct QuotedTrade {
    pub shape: TradeShape,
    pub quote: SolvedAmount,
}

impl QuotedTrade {
    /// The output this route was quoted at, the baseline every offset is measured against.
    pub(crate) fn quoted_amount_out(&self) -> U256 {
        self.quote.amount_out
    }
}

/// Where a measurement is built from: the quoted trade plus this offset's two outcomes.
#[derive(Debug, Clone)]
pub(crate) struct Measurement {
    pub round_id: u64,
    pub offset: u32,
    pub quote_block: u64,
    pub measured_block: u64,
    /// The replayed output, or why the replay produced none.
    pub replayed: Result<U256, ReplayFailure>,
    /// The fresh solve's full outcome, kept whole (not just its `amount_out`) so its route can be
    /// recorded alongside its amount. `None` when it found no route.
    pub fresh: Option<SolvedAmount>,
}

impl DecayRecord {
    /// Project a measurement into a record, computing the decomposition when both legs are present
    /// and non-zero.
    pub(crate) fn build(trade: &QuotedTrade, measurement: Measurement) -> Self {
        let Measurement { round_id, offset, quote_block, measured_block, replayed, fresh } =
            measurement;
        let replayed_amount = replayed.ok();
        let fresh_amount = fresh.as_ref().map(|f| f.amount_out);
        let bps = match (replayed_amount, fresh_amount) {
            (Some(replayed), Some(fresh)) => {
                DecayBps::new(trade.quoted_amount_out(), replayed, fresh)
            }
            _ => None,
        };
        // A failed replay is the more specific diagnosis, so it wins over a missing fresh solve,
        // which in turn wins over a decomposition that silently rejected a zero amount — every
        // record ends up with exactly one of `bps` or `failure`.
        let failure = match (&replayed, &fresh) {
            (Err(failure), _) => Some(*failure),
            (Ok(_), None) => Some(ReplayFailure::NoMarketReference),
            (Ok(_), Some(_)) if bps.is_none() => Some(ReplayFailure::ZeroAmount),
            (Ok(_), Some(_)) => None,
        };
        let fresh_route = fresh
            .as_ref()
            .and_then(|f| f.solved_route.as_deref())
            .map(render_route);
        Self {
            round_id,
            offset,
            quote_block,
            measured_block,
            token_in: trade.shape.token_in,
            token_out: trade.shape.token_out,
            amount_in: trade.shape.amount_in.to_string(),
            quoted_amount_out: trade.quoted_amount_out().to_string(),
            replayed_amount_out: replayed_amount.map(|a| a.to_string()),
            fresh_amount_out: fresh_amount.map(|a| a.to_string()),
            bps,
            failure,
            quote_algorithm: trade.quote.algorithm.clone(),
            fresh_route,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> TradeShape {
        TradeShape {
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000),
        }
    }

    fn trade(quoted: u64) -> QuotedTrade {
        QuotedTrade {
            shape: shape(),
            quote: SolvedAmount {
                amount_out: U256::from(quoted),
                amount_out_net_gas: U256::from(quoted),
                gas_estimate: U256::from(21_000),
                algorithm: "bellman_ford".to_string(),
                quote_json: None,
                solved_route: None,
            },
        }
    }

    fn measurement(
        replayed: Result<U256, ReplayFailure>,
        fresh: Option<SolvedAmount>,
    ) -> Measurement {
        Measurement {
            round_id: 3,
            offset: 2,
            quote_block: 100,
            measured_block: 102,
            replayed,
            fresh,
        }
    }

    /// A fresh solve's outcome carrying just an amount and no route — the common case for tests
    /// exercising the decomposition rather than the route projection (see
    /// `fresh_route_is_rendered_from_the_fresh_solves_route`).
    fn fresh_solve(amount_out: u64) -> SolvedAmount {
        SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(amount_out),
            gas_estimate: U256::from(21_000),
            algorithm: "most_liquid".to_string(),
            quote_json: None,
            solved_route: None,
        }
    }

    #[test]
    fn decomposition_identity_holds_when_not_clamped() {
        // Whatever the inputs, the two parts must sum back to the total — as long as the raw
        // execution slippage is not positive (see execution_slippage_is_clamped_at_zero_when_...
        // below for the clamped case, where the identity does not hold by design).
        let bps = DecayBps::new(U256::from(10_000), U256::from(9_900), U256::from(9_950))
            .expect("decomposable");
        assert!(!bps.execution_slippage_clamped);
        assert!(
            (bps.route_slippage_bps - (bps.market_movement_bps + bps.execution_slippage_bps)).abs() <
                1e-9
        );
    }

    #[test]
    fn positive_means_surplus() {
        // Replayed above the quote is a surplus, matching resolve::compare::slippage's convention.
        let up = DecayBps::new(U256::from(10_000), U256::from(10_050), U256::from(10_000))
            .expect("decomposable");
        assert!((up.route_slippage_bps - 50.0).abs() < 0.01, "got {}", up.route_slippage_bps);

        let down = DecayBps::new(U256::from(10_000), U256::from(9_900), U256::from(10_000))
            .expect("decomposable");
        assert!((down.route_slippage_bps + 100.0).abs() < 0.01, "got {}", down.route_slippage_bps);
    }

    #[test]
    fn market_movement_absorbs_a_drift_both_legs_saw() {
        // The whole move was market drift: a fresh solve at the later block fell just as far, so
        // nothing is attributable to the route being stale.
        let bps = DecayBps::new(U256::from(10_000), U256::from(9_900), U256::from(9_900))
            .expect("decomposable");
        assert!((bps.market_movement_bps + 100.0).abs() < 0.01);
        assert!(bps.execution_slippage_bps.abs() < 1e-9, "drift must not count as execution loss");
    }

    #[test]
    fn execution_slippage_isolates_the_stale_route() {
        // The market did not move (fresh == quoted) but the held route lost 100 bps: all of it is
        // execution slippage.
        let bps = DecayBps::new(U256::from(10_000), U256::from(9_900), U256::from(10_000))
            .expect("decomposable");
        assert!(bps.market_movement_bps.abs() < 1e-9);
        assert!((bps.execution_slippage_bps + 100.0).abs() < 0.01);
    }

    #[test]
    fn zero_quote_is_not_decomposable() {
        assert_eq!(DecayBps::new(U256::ZERO, U256::from(10), U256::from(10)), None);
    }

    #[test]
    fn execution_slippage_is_clamped_at_zero_when_the_stale_route_beats_a_fresh_solve() {
        // The held route (replayed 9_950) outperforms a fresh solve (9_900) at the same state.
        // That is solver noise, not a real opportunity: a real in-block solver already holds the
        // better of the two, so the physical outcome is never worse than the held route.
        let bps = DecayBps::new(U256::from(10_000), U256::from(9_950), U256::from(9_900))
            .expect("decomposable");
        assert!(bps.execution_slippage_clamped);
        assert!(
            bps.execution_slippage_bps.abs() < 1e-9,
            "must clamp to zero, got {}",
            bps.execution_slippage_bps
        );
    }

    #[test]
    fn missing_state_classifies_as_pool_gone() {
        // The exact wording fynd_core::ReplayError::MissingState renders.
        assert_eq!(
            ReplayFailure::classify("re-execution failed: no simulation state for component 0xabc"),
            ReplayFailure::PoolGone
        );
        assert_eq!(
            ReplayFailure::classify("re-execution failed: simulation failed on component 0xabc"),
            ReplayFailure::SimulationFailed
        );
    }

    #[test]
    fn missing_state_error_renders_to_the_string_classify_matches_on() {
        // Guards the wording `classify` depends on against the real `fynd_core` error rather than
        // a hand-written copy: if `ReplayError::MissingState`'s `#[error(...)]` attribute is ever
        // reworded, this test — not just the one above — must catch it.
        let error = fynd_core::ReplayError::MissingState("0xabc".to_string());
        let rendered = format!("re-execution failed: {error}");
        assert_eq!(ReplayFailure::classify(&rendered), ReplayFailure::PoolGone);
    }

    #[test]
    fn record_carries_bps_when_both_legs_are_present() {
        let record = DecayRecord::build(
            &trade(10_000),
            measurement(Ok(U256::from(9_900)), Some(fresh_solve(9_950))),
        );
        assert!(record.bps.is_some());
        assert_eq!(record.failure, None);
        assert_eq!(record.offset, 2);
        assert_eq!(record.measured_block, 102);
        assert_eq!(record.replayed_amount_out.as_deref(), Some("9900"));
        assert_eq!(record.fresh_amount_out.as_deref(), Some("9950"));
    }

    #[test]
    fn record_reports_the_replay_failure_over_a_missing_reference() {
        // Both legs missing: the replay failure is the more specific diagnosis.
        let record =
            DecayRecord::build(&trade(10_000), measurement(Err(ReplayFailure::PoolGone), None));
        assert_eq!(record.failure, Some(ReplayFailure::PoolGone));
        assert_eq!(record.bps, None);
        assert_eq!(record.replayed_amount_out, None);
    }

    #[test]
    fn record_reports_a_missing_market_reference() {
        let record = DecayRecord::build(&trade(10_000), measurement(Ok(U256::from(9_900)), None));
        assert_eq!(record.failure, Some(ReplayFailure::NoMarketReference));
        assert_eq!(record.bps, None);
        // The replayed amount is still recorded — only the decomposition is missing.
        assert_eq!(record.replayed_amount_out.as_deref(), Some("9900"));
    }

    #[test]
    fn a_zero_replayed_amount_is_reported_as_a_failure_not_silently_dropped() {
        // A replayed amount of zero is the extreme decay case — the held route became worthless —
        // and DecayBps::new rejects the zero, so without this classification the record would
        // carry neither `bps` nor `failure`, contradicting DecayRecord's own invariant and
        // silently dropping the worst observations from the tail statistics.
        let record = DecayRecord::build(
            &trade(10_000),
            measurement(Ok(U256::ZERO), Some(fresh_solve(9_950))),
        );
        assert_eq!(record.failure, Some(ReplayFailure::ZeroAmount));
        assert_eq!(record.bps, None);
        // The zero is still recorded — only the decomposition is missing.
        assert_eq!(record.replayed_amount_out.as_deref(), Some("0"));
    }

    #[test]
    fn fresh_route_is_rendered_from_the_fresh_solves_route() {
        let fresh = SolvedAmount {
            solved_route: Some(Box::new(crate::resolve::test_support::route(&[(
                "uniswap_v2",
                "USDT",
                "DAI",
            )]))),
            ..fresh_solve(9_950)
        };
        let record =
            DecayRecord::build(&trade(10_000), measurement(Ok(U256::from(9_900)), Some(fresh)));
        assert_eq!(record.fresh_route.as_deref(), Some("USDT -[uniswap_v2]-> DAI"));
        // The quote's own algorithm is still carried, distinctly named from the fresh route.
        assert_eq!(record.quote_algorithm, "bellman_ford");
    }

    #[test]
    fn fresh_route_is_absent_when_the_fresh_solve_found_none() {
        let record = DecayRecord::build(&trade(10_000), measurement(Ok(U256::from(9_900)), None));
        assert_eq!(record.fresh_route, None);
    }

    #[test]
    fn json_omits_absent_fields_and_keeps_amounts_as_strings() {
        let record =
            DecayRecord::build(&trade(10_000), measurement(Err(ReplayFailure::PoolGone), None));
        let json = serde_json::to_string(&record).expect("serializable");
        assert!(json.contains(r#""failure":"pool_gone""#), "{json}");
        assert!(json.contains(r#""amount_in":"1000""#), "{json}");
        assert!(!json.contains("replayed_amount_out"), "absent legs must be omitted: {json}");
        assert!(!json.contains(r#""bps""#), "{json}");
    }
}
