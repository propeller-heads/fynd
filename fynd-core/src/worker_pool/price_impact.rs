//! Calculates signed quote price impact as
//! `1 - executed_output / spot_reference_output`.
//!
//! A negative value means favorable execution. Each swap contributes the spot price from
//! the component state stored in that swap. The calculation processes split routes as
//! token-flow graphs. If the calculation fails, the worker omits `price_impact_bps`, logs
//! the reason, and counts the outcome.
//!
//! # Limitation: the reference is the reported spot price, not a marginal output rate
//!
//! Tycho documents `ProtocolSim::spot_price(base, quote)` as a fee-marked-up buy price. Fynd calls
//! `ProtocolSim::spot_price(token_in, token_out)` in the executed direction. Tycho implementations
//! differ in direction and fee convention, so `price_impact_bps` can carry a venue-dependent fee
//! bias.

use std::fmt;

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::{token::Token, Address};

use crate::types::{quote::branch_collections, ComponentId, Route, Swap};

/// Which amount in the price-impact calculation a value refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AmountSource {
    RouteInput,
    RouteOutput,
    SwapInput,
}

impl fmt::Display for AmountSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AmountSource::RouteInput => f.write_str("route input"),
            AmountSource::RouteOutput => f.write_str("route output"),
            AmountSource::SwapInput => f.write_str("swap input"),
        }
    }
}

/// Why a quote carries no price impact.
///
/// None of these fail the quote: the worker logs the reason, counts it, and leaves
/// `price_impact_bps` unset.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PriceImpactError {
    #[error("route has no swaps")]
    EmptyRoute,
    #[error("the route's token map has no entry for {0}")]
    UnknownToken(Address),
    #[error(
        "component {component_id} could not report spot_price for ({token_in}, {token_out}): \
         {reason}"
    )]
    SpotPriceQueryFailed {
        component_id: ComponentId,
        token_in: Address,
        token_out: Address,
        reason: String,
    },
    #[error("{kind} amount {amount} cannot be represented as a finite f64")]
    AmountOutOfRange { kind: AmountSource, amount: BigUint },
    #[error("a swap consumes {0} before any swap produced it")]
    UnfedToken(Address),
    #[error("swaps from token {0} consume zero input")]
    ZeroConsumedInput(Address),
    #[error(
        "spot_price for ({token_in}, {token_out}) returned {price}, which is not a positive \
         finite number"
    )]
    InvalidSpotPrice { token_in: Address, token_out: Address, price: f64 },
    #[error("the route's output at spot prices is not a positive finite number")]
    NoReferenceOutput,
    #[error("price impact {impact} cannot be represented as basis points in an i32")]
    BasisPointsOutOfRange { impact: f64 },
}

impl PriceImpactError {
    /// A bounded label for the outcome metric: one value per variant.
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            PriceImpactError::EmptyRoute => "empty_route",
            PriceImpactError::UnknownToken(_) => "unknown_token",
            PriceImpactError::SpotPriceQueryFailed { .. } => "spot_price_query_failed",
            PriceImpactError::AmountOutOfRange { .. } => "amount_out_of_range",
            PriceImpactError::UnfedToken(_) => "unfed_token",
            PriceImpactError::ZeroConsumedInput(_) => "zero_consumed_input",
            PriceImpactError::InvalidSpotPrice { .. } => "invalid_spot_price",
            PriceImpactError::NoReferenceOutput => "no_reference_output",
            PriceImpactError::BasisPointsOutOfRange { .. } => "basis_points_out_of_range",
        }
    }
}

/// One swap processed by [`spot_reference_output`].
pub(crate) struct SpotLeg<'a> {
    pub token_in: &'a Address,
    pub token_out: &'a Address,
    /// Raw units of `token_in` the swap consumed.
    pub amount_in_raw: f64,
    /// The spot price returned for the `(token_in, token_out)` arguments by the component state
    /// used to simulate this swap, in human `token_out` per human `token_in`.
    pub reported_spot_price: f64,
}

/// What the route as a whole took in and paid out.
pub(crate) struct PriceImpactInputs<'a> {
    pub token_in: &'a Address,
    pub token_out: &'a Address,
    pub amount_in_raw: &'a BigUint,
    pub amount_out_raw: &'a BigUint,
    pub input_decimals: u32,
    pub output_decimals: u32,
}

/// Computes signed price impact as `1 - executed_output / spot_reference_output`; negative
/// values represent favorable execution.
///
/// [`spot_reference_output`] seeds the input token with the route's input and visits the legs one
/// branch collection at a time. Each leg takes its share of the reference amount standing at its
/// input token — its share of what the route actually consumed there — and multiplies it by its
/// reported spot price into its output token. Legs into the route's output token accumulate the
/// reference output. On a linear route every share is one and the reference is the input times the
/// product of the spot prices; on a split route a deviation at an earlier hop compounds through the
/// hops it feeds.
///
/// Shares are ratios of raw amounts and spot prices are in human units, so decimals enter only at
/// the two endpoints.
pub(crate) fn price_impact_from_spot_legs(
    legs: &[SpotLeg<'_>],
    inputs: &PriceImpactInputs<'_>,
) -> Result<f64, PriceImpactError> {
    if legs.is_empty() {
        return Err(PriceImpactError::EmptyRoute);
    }
    let spot_reference_output_human = spot_reference_output(legs, inputs)?;
    let executed_output_human = raw_to_human_units(
        inputs.amount_out_raw,
        inputs.output_decimals,
        AmountSource::RouteOutput,
    )?;
    Ok(1.0 - executed_output_human / spot_reference_output_human)
}

/// Converts signed price impact to basis points without saturating the wire-format `i32`.
pub(crate) fn price_impact_to_basis_points(impact: f64) -> Result<i32, PriceImpactError> {
    let rounded_basis_points = (impact * 10_000.0).round();
    if !rounded_basis_points.is_finite() ||
        rounded_basis_points < f64::from(i32::MIN) ||
        rounded_basis_points > f64::from(i32::MAX)
    {
        return Err(PriceImpactError::BasisPointsOutOfRange { impact });
    }
    Ok(rounded_basis_points as i32)
}

/// What the route would pay at spot prices, in human units of its output token.
fn spot_reference_output(
    legs: &[SpotLeg<'_>],
    inputs: &PriceImpactInputs<'_>,
) -> Result<f64, PriceImpactError> {
    let mut reference_human_by_token: FxHashMap<&Address, f64> = FxHashMap::default();
    reference_human_by_token.insert(
        inputs.token_in,
        raw_to_human_units(inputs.amount_in_raw, inputs.input_decimals, AmountSource::RouteInput)?,
    );
    let mut reference_output_human = 0.0_f64;

    for (token_in, collection) in branch_collections(legs, |leg| leg.token_in) {
        let available_reference_input_human = reference_human_by_token
            .get(&token_in)
            .copied()
            .ok_or_else(|| PriceImpactError::UnfedToken(token_in.clone()))?;
        let consumed_input_raw_total: f64 = collection
            .iter()
            .map(|leg| leg.amount_in_raw)
            .sum();
        if consumed_input_raw_total <= 0.0 {
            return Err(PriceImpactError::ZeroConsumedInput(token_in));
        }
        for leg in collection {
            if !(leg.reported_spot_price.is_finite() && leg.reported_spot_price > 0.0) {
                return Err(PriceImpactError::InvalidSpotPrice {
                    token_in: leg.token_in.clone(),
                    token_out: leg.token_out.clone(),
                    price: leg.reported_spot_price,
                });
            }
            let share = leg.amount_in_raw / consumed_input_raw_total;
            let reference_output_contribution_human =
                available_reference_input_human * share * leg.reported_spot_price;
            if leg.token_out == inputs.token_out {
                reference_output_human += reference_output_contribution_human;
            } else {
                *reference_human_by_token
                    .entry(leg.token_out)
                    .or_insert(0.0) += reference_output_contribution_human;
            }
        }
    }

    if reference_output_human.is_finite() && reference_output_human > 0.0 {
        Ok(reference_output_human)
    } else {
        Err(PriceImpactError::NoReferenceOutput)
    }
}

fn raw_to_human_units(
    raw: &BigUint,
    decimals: u32,
    kind: AmountSource,
) -> Result<f64, PriceImpactError> {
    let raw_f64 = raw
        .to_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| PriceImpactError::AmountOutOfRange { kind, amount: raw.clone() })?;
    Ok(raw_f64 / 10f64.powi(decimals as i32))
}

/// The spot price returned for the `(token_in, token_out)` arguments by the component state
/// used to simulate this swap, in human `token_out` per human `token_in`.
///
/// This is the one place the reference comes from. Tycho's cross-implementation differences
/// in direction and fee semantics are documented in the module-level limitation.
fn reported_spot_price(
    swap: &Swap,
    token_in: &Token,
    token_out: &Token,
) -> Result<f64, PriceImpactError> {
    swap.protocol_state()
        .spot_price(token_in, token_out)
        .map_err(|err| PriceImpactError::SpotPriceQueryFailed {
            component_id: swap.component_id().to_string(),
            token_in: token_in.address.clone(),
            token_out: token_out.address.clone(),
            reason: err.to_string(),
        })
}

/// Price impact of a route from each swap's reported spot price.
///
/// Token decimals come from the route's own token map, which every built-in algorithm fills in
/// for every swap, so the calculation touches no shared market state.
pub(crate) fn route_price_impact(
    route: &Route,
    amount_in_raw: &BigUint,
    amount_out_raw: &BigUint,
) -> Result<f64, PriceImpactError> {
    let swaps = route.swaps();
    let (Some(first), Some(last)) = (swaps.first(), swaps.last()) else {
        return Err(PriceImpactError::EmptyRoute);
    };
    let token_of = |address: &Address| {
        route
            .tokens()
            .get(address)
            .ok_or_else(|| PriceImpactError::UnknownToken(address.clone()))
    };

    let mut legs = Vec::with_capacity(swaps.len());
    for swap in swaps {
        let reported_spot_price =
            reported_spot_price(swap, token_of(swap.token_in())?, token_of(swap.token_out())?)?;
        let swap_amount_in_raw = swap
            .amount_in()
            .to_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| PriceImpactError::AmountOutOfRange {
                kind: AmountSource::SwapInput,
                amount: swap.amount_in().clone(),
            })?;
        legs.push(SpotLeg {
            token_in: swap.token_in(),
            token_out: swap.token_out(),
            amount_in_raw: swap_amount_in_raw,
            reported_spot_price,
        });
    }

    let inputs = PriceImpactInputs {
        token_in: first.token_in(),
        token_out: last.token_out(),
        amount_in_raw,
        amount_out_raw,
        input_decimals: token_of(first.token_in())?.decimals,
        output_decimals: token_of(last.token_out())?.decimals,
    };
    price_impact_from_spot_legs(&legs, &inputs)
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::tycho_core::{
        dto::ProtocolStateDelta,
        models::token::Token,
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        },
        Bytes,
    };

    use super::*;
    use crate::algorithm::test_utils::{
        addr, component, token, token_with_decimals, MockProtocolSim,
    };

    fn parse_biguint(s: &str) -> BigUint {
        s.parse().unwrap()
    }

    fn inputs<'a>(
        token_in: &'a Address,
        token_out: &'a Address,
        amount_in_raw: &'a BigUint,
        amount_out_raw: &'a BigUint,
    ) -> PriceImpactInputs<'a> {
        PriceImpactInputs {
            token_in,
            token_out,
            amount_in_raw,
            amount_out_raw,
            input_decimals: 0,
            output_decimals: 0,
        }
    }

    #[test]
    fn test_single_hop_near_one_to_one() {
        // 1_000_000 DAI (18dp) -> 999_843.73 USDC (6dp), spot 0.9998 USDC/DAI.
        let dai = addr(0x01);
        let usdc = addr(0x02);
        let legs = [SpotLeg {
            token_in: &dai,
            token_out: &usdc,
            amount_in_raw: 1e24,
            reported_spot_price: 0.9998,
        }];
        let (amount_in, amount_out) =
            (parse_biguint("1000000000000000000000000"), parse_biguint("999843730000"));
        let impact_inputs = PriceImpactInputs {
            input_decimals: 18,
            output_decimals: 6,
            ..inputs(&dai, &usdc, &amount_in, &amount_out)
        };
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!(impact.abs() < 0.001, "expected ~0 impact, got {impact}");
    }

    #[test]
    fn test_linear_route_multiplies_reported_spot_prices() {
        // A->B at 2.0 then B->C at 0.5 is a reference of 1:1; 1000 in, 980 out => 2% impact.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [
            SpotLeg {
                token_in: &a,
                token_out: &b,
                amount_in_raw: 1000.0,
                reported_spot_price: 2.0,
            },
            SpotLeg {
                token_in: &b,
                token_out: &c,
                amount_in_raw: 1990.0,
                reported_spot_price: 0.5,
            },
        ];
        let (amount_in, amount_out) = (parse_biguint("1000"), parse_biguint("980"));
        let impact_inputs = inputs(&a, &c, &amount_in, &amount_out);
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!((impact - 0.02).abs() < 1e-9, "got {impact}");
    }

    #[test]
    fn test_favorable_execution_is_negative() {
        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [SpotLeg {
            token_in: &a,
            token_out: &b,
            amount_in_raw: 100.0,
            reported_spot_price: 1.0,
        }];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("101"));
        let impact_inputs = inputs(&a, &b, &amount_in, &amount_out);
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!(impact < 0.0, "got {impact}");
    }

    #[test]
    fn test_basis_points_conversion_rejects_unrepresentable_impact() {
        // Favorable execution below -100% is mathematically valid and must not be clipped.
        assert_eq!(price_impact_to_basis_points(-1.5).unwrap(), -15_000);

        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [SpotLeg {
            token_in: &a,
            token_out: &b,
            amount_in_raw: 1.0,
            reported_spot_price: 1e-300,
        }];
        let (amount_in, amount_out) = (parse_biguint("1"), parse_biguint("1"));
        let impact_inputs = inputs(&a, &b, &amount_in, &amount_out);
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();

        let err = price_impact_to_basis_points(impact).unwrap_err();
        assert_eq!(err.outcome(), "basis_points_out_of_range");
        let PriceImpactError::BasisPointsOutOfRange { impact: rejected } = err else {
            panic!("expected BasisPointsOutOfRange, got {err}");
        };
        assert_eq!(rejected, impact);
    }

    #[test]
    fn test_zero_reported_spot_price() {
        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [SpotLeg {
            token_in: &a,
            token_out: &b,
            amount_in_raw: 100.0,
            reported_spot_price: 0.0,
        }];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("100"));
        let impact_inputs = inputs(&a, &b, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap_err();
        let PriceImpactError::InvalidSpotPrice { price, .. } = err else {
            panic!("expected InvalidSpotPrice, got {err}");
        };
        assert_eq!(price, 0.0);
    }

    #[test]
    fn test_endpoint_decimals() {
        // 1 WETH (18dp) at spot 2000 USDC/WETH -> 2000 USDC (6dp): no impact.
        let weth = addr(0x01);
        let usdc = addr(0x02);
        let legs = [SpotLeg {
            token_in: &weth,
            token_out: &usdc,
            amount_in_raw: 1e18,
            reported_spot_price: 2000.0,
        }];
        let (amount_in, amount_out) =
            (parse_biguint("1000000000000000000"), parse_biguint("2000000000"));
        let impact_inputs = PriceImpactInputs {
            input_decimals: 18,
            output_decimals: 6,
            ..inputs(&weth, &usdc, &amount_in, &amount_out)
        };
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!(impact.abs() < 1e-6, "got {impact}");
    }

    #[test]
    fn test_amount_beyond_f64_names_its_source() {
        // A 1e400 amount cannot be represented as a finite f64; the calculation must refuse it
        // rather than divide by inf.
        let a = addr(0x01);
        let b = addr(0x02);
        let huge = "1".to_string() + &"0".repeat(400);
        let legs =
            [SpotLeg { token_in: &a, token_out: &b, amount_in_raw: 1.0, reported_spot_price: 1.0 }];
        let (amount_in, amount_out) = (parse_biguint(&huge), parse_biguint("1"));
        let impact_inputs = inputs(&a, &b, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap_err();
        let PriceImpactError::AmountOutOfRange { kind, .. } = err else {
            panic!("expected AmountOutOfRange, got {err}");
        };
        assert_eq!(kind, AmountSource::RouteInput);
    }

    #[test]
    fn test_parallel_split_weights_branches_by_amount() {
        // 100 A split 60/40 across two A->B pools at spot 2.0 and 3.0: reference 120 + 120 = 240,
        // actual 114 + 114 = 228 => 5% impact. A per-hop product cannot express this at all.
        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [
            SpotLeg { token_in: &a, token_out: &b, amount_in_raw: 60.0, reported_spot_price: 2.0 },
            SpotLeg { token_in: &a, token_out: &b, amount_in_raw: 40.0, reported_spot_price: 3.0 },
        ];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("228"));
        let impact_inputs = inputs(&a, &b, &amount_in, &amount_out);
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!((impact - 0.05).abs() < 1e-9, "got {impact}");
    }

    #[test]
    fn test_tree_split_compounds_intermediate_deviation() {
        // 100 A: 50 straight to C at spot 2.0 (reference 100), 50 to B at spot 1.0 (reference 50,
        // paid 49), then B split 30/19 into two B->C pools at spot 2.0. The B collection is
        // seeded with the reference 50, not the 49 paid, so the reference at C is 100 + 100 =
        // 200. Actual 95 + 55 + 36 = 186 => 7% impact.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [
            SpotLeg { token_in: &a, token_out: &c, amount_in_raw: 50.0, reported_spot_price: 2.0 },
            SpotLeg { token_in: &a, token_out: &b, amount_in_raw: 50.0, reported_spot_price: 1.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in_raw: 30.0, reported_spot_price: 2.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in_raw: 19.0, reported_spot_price: 2.0 },
        ];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("186"));
        let impact_inputs = inputs(&a, &c, &amount_in, &amount_out);
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!((impact - 0.07).abs() < 1e-9, "got {impact}");
    }

    #[test]
    fn test_ungrouped_swap_order() {
        // A token-flow graph whose legs are not grouped by input token: the second A leg is listed
        // after the C->D leg that consumes what it produces. Walking collection by collection,
        // C is seeded with both A->C and B->C (reference 100) before C->D runs: reference D =
        // 200, actual 190 => 5% impact. Walking in list order would seed C with 50 and report
        // -90%.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let d = addr(0x04);
        let legs = [
            SpotLeg { token_in: &a, token_out: &b, amount_in_raw: 50.0, reported_spot_price: 1.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in_raw: 49.0, reported_spot_price: 1.0 },
            SpotLeg { token_in: &c, token_out: &d, amount_in_raw: 97.0, reported_spot_price: 2.0 },
            SpotLeg { token_in: &a, token_out: &c, amount_in_raw: 50.0, reported_spot_price: 1.0 },
        ];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("190"));
        let impact_inputs = inputs(&a, &d, &amount_in, &amount_out);
        let impact = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap();
        assert!((impact - 0.05).abs() < 1e-9, "got {impact}");
    }

    #[test]
    fn test_consumer_before_producer() {
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [SpotLeg {
            token_in: &b,
            token_out: &c,
            amount_in_raw: 10.0,
            reported_spot_price: 1.0,
        }];
        let (amount_in, amount_out) = (parse_biguint("10"), parse_biguint("10"));
        let impact_inputs = inputs(&a, &c, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap_err();
        let PriceImpactError::UnfedToken(unfed) = err else {
            panic!("expected UnfedToken, got {err}");
        };
        assert_eq!(unfed, b);
    }

    #[test]
    fn test_zero_consumed_input() {
        // The only swap out of A consumes nothing, so no share can be attributed.
        let a = addr(0x01);
        let b = addr(0x02);
        let legs =
            [SpotLeg { token_in: &a, token_out: &b, amount_in_raw: 0.0, reported_spot_price: 1.0 }];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("100"));
        let impact_inputs = inputs(&a, &b, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap_err();
        let PriceImpactError::ZeroConsumedInput(token) = err else {
            panic!("expected ZeroConsumedInput, got {err}");
        };
        assert_eq!(token, a);
    }

    #[test]
    fn test_flow_that_never_reaches_output_token() {
        // A->B is valid flow, but the route's output token is C, which nothing feeds.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [SpotLeg {
            token_in: &a,
            token_out: &b,
            amount_in_raw: 100.0,
            reported_spot_price: 1.0,
        }];
        let (amount_in, amount_out) = (parse_biguint("100"), parse_biguint("100"));
        let impact_inputs = inputs(&a, &c, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &impact_inputs).unwrap_err();
        let PriceImpactError::NoReferenceOutput = err else {
            panic!("expected NoReferenceOutput, got {err}");
        };
    }

    /// Builds a swap with the component state stored by the route. Price-impact calculation
    /// reads the reported spot price from this state.
    fn route_swap(
        pool_id: &str,
        token_in: &Token,
        token_out: &Token,
        amount_in: u64,
        amount_out: u64,
        state: Box<dyn ProtocolSim>,
        split: f64,
    ) -> Swap {
        Swap::new(
            pool_id.to_string(),
            "mock".to_string(),
            token_in.address.clone(),
            token_out.address.clone(),
            BigUint::from(amount_in),
            BigUint::from(amount_out),
            BigUint::ZERO,
            component(pool_id, &[token_in.clone(), token_out.clone()]),
            state,
        )
        .with_split(split)
    }

    fn token_map(tokens: &[&Token]) -> Vec<(Bytes, Token)> {
        tokens
            .iter()
            .map(|t| (t.address.clone(), (*t).clone()))
            .collect()
    }

    #[test]
    fn test_route_price_impact_for_split_route() {
        // 100 A split 60/40 over two pools at spot 2.0, paying 114 and 78: 192 of a reference
        // 200. `MockProtocolSim` quotes `spot` from the lower to the higher address.
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let route = Route::new(
            vec![
                route_swap("p1", &a, &b, 60, 114, Box::new(MockProtocolSim::new(2.0)), 0.6),
                route_swap("p2", &a, &b, 40, 78, Box::new(MockProtocolSim::new(2.0)), 0.0),
            ],
            token_map(&[&a, &b]),
        )
        .unwrap();

        let impact =
            route_price_impact(&route, &parse_biguint("100"), &parse_biguint("192")).unwrap();
        assert!((impact - 0.04).abs() < 1e-9, "got {impact}");
    }

    #[test]
    fn test_route_price_impact_reads_decimals_from_route_tokens() {
        // 1 A (18dp) -> 2 B (6dp) at spot 2.0: the raw amounts differ by 1e12 but the impact is 0.
        let a = token_with_decimals(0x01, "A", 18);
        let b = token_with_decimals(0x02, "B", 6);
        let route = Route::new(
            vec![route_swap(
                "p1",
                &a,
                &b,
                1_000_000_000_000_000_000,
                2_000_000,
                Box::new(MockProtocolSim::new(2.0)),
                0.0,
            )],
            token_map(&[&a, &b]),
        )
        .unwrap();

        let impact = route_price_impact(
            &route,
            &parse_biguint("1000000000000000000"),
            &parse_biguint("2000000"),
        )
        .unwrap();
        assert!(impact.abs() < 1e-9, "got {impact}");
    }

    #[test]
    fn test_route_price_impact_without_token_map() {
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let route = Route::new(
            vec![route_swap("p1", &a, &b, 100, 200, Box::new(MockProtocolSim::new(2.0)), 0.0)],
            Vec::new(),
        )
        .unwrap();

        let err =
            route_price_impact(&route, &parse_biguint("100"), &parse_biguint("200")).unwrap_err();
        let PriceImpactError::UnknownToken(unknown) = err else {
            panic!("expected UnknownToken, got {err}");
        };
        assert_eq!(unknown, a.address);
    }

    #[test]
    fn test_spot_price_failure_names_the_leg() {
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let route = Route::new(
            vec![route_swap("p1", &a, &b, 100, 200, Box::new(SellRateSim::unpriced()), 0.0)],
            token_map(&[&a, &b]),
        )
        .unwrap();

        let err =
            route_price_impact(&route, &parse_biguint("100"), &parse_biguint("200")).unwrap_err();
        let PriceImpactError::SpotPriceQueryFailed { component_id, token_in, token_out, .. } = err
        else {
            panic!("expected SpotPriceQueryFailed, got {err}");
        };
        assert_eq!(component_id, "p1");
        assert_eq!(token_in, a.address);
        assert_eq!(token_out, b.address);
    }

    /// How the fixture responds to `spot_price`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    enum SpotPriceBehavior {
        /// The sell-side rate the pool pays: `mid * (1 - fee)` from the lower to the higher
        /// address.
        SellRate,
        /// Returns an error to model a venue that does not provide a spot price.
        Unavailable,
    }

    /// A no-slippage pool for exercising Fynd's own math: `get_amount_out` pays
    /// `amount_in * mid * (1 - fee)` and `spot_price` reports that same rate, so any impact comes
    /// from what the route paid against what `spot_reference_output` expected. It models no real
    /// venue.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct SellRateSim {
        mid: f64,
        fee: f64,
        spot: SpotPriceBehavior,
    }

    impl SellRateSim {
        fn new(mid: f64, fee: f64) -> Self {
            Self { mid, fee, spot: SpotPriceBehavior::SellRate }
        }

        fn unpriced() -> Self {
            Self { mid: 1.0, fee: 0.0, spot: SpotPriceBehavior::Unavailable }
        }

        fn rate_for(&self, token_in: &Token, token_out: &Token) -> f64 {
            let mid = if token_in.address < token_out.address { self.mid } else { 1.0 / self.mid };
            mid * (1.0 - self.fee)
        }
    }

    #[typetag::serde]
    impl ProtocolSim for SellRateSim {
        fn fee(&self) -> f64 {
            self.fee
        }

        fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
            match self.spot {
                SpotPriceBehavior::SellRate => Ok(self.rate_for(base, quote)),
                SpotPriceBehavior::Unavailable => {
                    Err(SimulationError::RecoverableError("spot price is unavailable".to_string()))
                }
            }
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            let rate = self.rate_for(token_in, token_out);
            let amount_out = (amount_in.to_f64().unwrap_or(0.0) * rate).round();
            Ok(GetAmountOutResult::new(
                BigUint::from(amount_out as u64),
                BigUint::ZERO,
                self.clone_box(),
            ))
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::from(u64::MAX), BigUint::from(u64::MAX)))
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &std::collections::HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("delta_transition is not needed by SellRateSim")
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .is_some_and(|o| o.mid == self.mid && o.fee == self.fee)
        }
    }

    #[test]
    fn test_no_slippage_sell_rate_pool_reports_zero_impact() {
        // Fynd expects the reference rate to include the fee. If the reference rate equals the
        // fee-inclusive rate that the pool paid, a 30 bps fee on a trade that does not move the
        // pool produces zero price impact. 1_000_000 A at mid 1.0 pays 997_000 B.
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let state = SellRateSim::new(1.0, 0.003);
        let paid = state
            .get_amount_out(BigUint::from(1_000_000u64), &a, &b)
            .unwrap()
            .amount;
        assert_eq!(paid, BigUint::from(997_000u64));
        let route = Route::new(
            vec![route_swap("p1", &a, &b, 1_000_000, 997_000, Box::new(state), 0.0)],
            token_map(&[&a, &b]),
        )
        .unwrap();

        let impact =
            route_price_impact(&route, &parse_biguint("1000000"), &parse_biguint("997000"))
                .unwrap();
        assert!(impact.abs() < 1e-9, "expected zero impact, got {impact}");
    }
}
