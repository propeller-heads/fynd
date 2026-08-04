//! One measurement of a quoted route replayed `offset` blocks later, and its JSONL projection.

use alloy::primitives::{Address, U256};
use fynd_tools_common::bps::raw_bps_diff;
use num_bigint::BigUint;
use serde::Serialize;

use crate::{decay::sample::TradeShape, resolve::SolvedAmount};

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
#[expect(
    clippy::struct_field_names,
    reason = "the _bps suffix is the unit, and dropping it from a struct of three bare f64s is \
              exactly how a bps figure gets mistaken for a ratio"
)]
pub(crate) struct DecayBps {
    /// The quoted route replayed at the later state, against its own original quote. The total
    /// move: market drift plus whatever the route lost by being stale.
    pub route_slippage_bps: f64,
    /// A freshly solved quote at the later state, against the original quote. The part of the move
    /// that any router would have suffered — unavoidable market drift.
    pub market_movement_bps: f64,
    /// `route_slippage_bps - market_movement_bps`: the part specific to holding a stale route,
    /// which better routing or faster submission could recover. Negative means the stale route
    /// underperformed a fresh solve.
    pub execution_slippage_bps: f64,
}

impl DecayBps {
    /// Build the decomposition from three amounts in the output token's units.
    ///
    /// Returns `None` when the original quote is zero, which leaves every ratio undefined.
    pub(crate) fn new(quoted: U256, replayed: U256, fresh: U256) -> Option<Self> {
        let route_slippage_bps = bps_against(quoted, replayed)?;
        let market_movement_bps = bps_against(quoted, fresh)?;
        Some(Self {
            route_slippage_bps,
            market_movement_bps,
            execution_slippage_bps: route_slippage_bps - market_movement_bps,
        })
    }
}

/// `later` versus `quoted`, in bps, positive when `later` is larger.
///
/// `raw_bps_diff(baseline, other)` computes `(baseline - other) / other`, so passing the later
/// amount as the baseline and the original quote as `other` yields "how much more the later state
/// produced", matching [`crate::resolve::compare::slippage`]'s convention.
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
    /// The algorithm that produced the original quote.
    pub algorithm: String,
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

/// Where a measurement is built from: the quoted trade plus this offset's two outputs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Measurement {
    pub round_id: u64,
    pub offset: u32,
    pub quote_block: u64,
    pub measured_block: u64,
    /// The replayed output, or why the replay produced none.
    pub replayed: Result<U256, ReplayFailure>,
    /// The fresh solve's output, or `None` when it found no route.
    pub fresh: Option<U256>,
}

impl DecayRecord {
    /// Project a measurement into a record, computing the decomposition when both legs are present.
    pub(crate) fn build(trade: &QuotedTrade, measurement: Measurement) -> Self {
        let replayed = measurement.replayed.ok();
        // A failed replay is the more specific diagnosis, so it wins over a missing fresh solve.
        let failure = match (measurement.replayed, measurement.fresh) {
            (Err(failure), _) => Some(failure),
            (Ok(_), None) => Some(ReplayFailure::NoMarketReference),
            (Ok(_), Some(_)) => None,
        };
        let bps = match (replayed, measurement.fresh) {
            (Some(replayed), Some(fresh)) => {
                DecayBps::new(trade.quoted_amount_out(), replayed, fresh)
            }
            _ => None,
        };
        Self {
            round_id: measurement.round_id,
            offset: measurement.offset,
            quote_block: measurement.quote_block,
            measured_block: measurement.measured_block,
            token_in: trade.shape.token_in,
            token_out: trade.shape.token_out,
            amount_in: trade.shape.amount_in.to_string(),
            quoted_amount_out: trade.quoted_amount_out().to_string(),
            replayed_amount_out: replayed.map(|a| a.to_string()),
            fresh_amount_out: measurement.fresh.map(|a| a.to_string()),
            bps,
            failure,
            algorithm: trade.quote.algorithm.clone(),
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

    fn measurement(replayed: Result<U256, ReplayFailure>, fresh: Option<U256>) -> Measurement {
        Measurement {
            round_id: 3,
            offset: 2,
            quote_block: 100,
            measured_block: 102,
            replayed,
            fresh,
        }
    }

    #[test]
    fn decomposition_identity_holds() {
        // Whatever the inputs, the two parts must sum back to the total.
        let bps = DecayBps::new(U256::from(10_000), U256::from(9_900), U256::from(9_950))
            .expect("decomposable");
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
    fn record_carries_bps_when_both_legs_are_present() {
        let record = DecayRecord::build(
            &trade(10_000),
            measurement(Ok(U256::from(9_900)), Some(U256::from(9_950))),
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
