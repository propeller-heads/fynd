//! Price-impact computation for quotes.
//!
//! The worker computes a quote's price impact here once the algorithm has returned its route: how
//! far the route's payout falls short of what the same input would fetch at the pre-trade spot
//! price of every pool it goes through. A split route is walked as the token-flow graph it is, so
//! a route from any algorithm gets the same definition.

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_common::models::{token::Token, Address};

use crate::{
    feed::market_data::{MarketData, MarketDataView},
    types::{quote::branch_collections, ComponentId, Route},
};

/// Why a quote carries no price impact.
///
/// None of these fail the quote: the worker logs the reason and leaves `price_impact_bps` unset.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PriceImpactError {
    #[error("route has no swaps")]
    EmptyRoute,
    #[error("market state is locked by a writer")]
    MarketBusy,
    #[error("token {0} is not in the market")]
    UnknownToken(Address),
    #[error("spot price of component {component_id} failed: {reason}")]
    SpotPrice { component_id: ComponentId, reason: String },
    #[error("amount {0} does not fit an f64")]
    AmountOutOfRange(BigUint),
    #[error("a swap consumes {0} before any swap produced it")]
    UnfedToken(Address),
    #[error("no input flows through {0}")]
    NoFlow(Address),
    #[error("spot price {spot} from {token_in} to {token_out} is not a positive finite number")]
    InvalidSpot { token_in: Address, token_out: Address, spot: f64 },
    #[error("the route's output at spot prices is not a positive finite number")]
    NoIdealOutput,
}

/// One swap as the price-impact walk sees it.
pub(crate) struct SpotLeg<'a> {
    pub token_in: &'a Address,
    pub token_out: &'a Address,
    /// Raw units of `token_in` the swap consumed.
    pub amount_in: f64,
    /// Human units of `token_out` per human unit of `token_in` before the trade, fee included.
    pub spot: f64,
}

/// What the route as a whole took in and paid out.
pub(crate) struct RouteEndpoints<'a> {
    pub token_in: &'a Address,
    pub token_out: &'a Address,
    pub amount_in: &'a BigUint,
    pub amount_out: &'a BigUint,
    pub decimals_in: u32,
    pub decimals_out: u32,
}

/// Signed price impact of a route: `1 - amount_out / ideal_out`, where `ideal_out` is what the
/// route would pay at the spot price of each leg.
///
/// The walk seeds the input token with `amount_in` and visits the legs one branch collection at
/// a time. Each leg takes its share of the ideal amount standing at its input token — its share
/// of what the route actually consumed there — and multiplies it by its spot price into its
/// output token. Legs into the route's output token accumulate `ideal_out`. On a linear route
/// every share is one and this is the product of the spot prices; on a split route the shortfall
/// of an earlier hop compounds through the hops it feeds.
///
/// Shares are ratios of raw amounts and spot prices are in human units, so decimals enter only at
/// the two endpoints.
pub(crate) fn price_impact_from_spot_legs(
    legs: &[SpotLeg<'_>],
    route: &RouteEndpoints<'_>,
) -> Result<f64, PriceImpactError> {
    if legs.is_empty() {
        return Err(PriceImpactError::EmptyRoute);
    }
    let ideal_out = ideal_output(legs, route)?;
    let exec_out = human_amount(route.amount_out, route.decimals_out)?;
    Ok(1.0 - exec_out / ideal_out)
}

/// What the route would pay at spot prices, in human units of its output token.
fn ideal_output(legs: &[SpotLeg<'_>], route: &RouteEndpoints<'_>) -> Result<f64, PriceImpactError> {
    let mut consumed_at: FxHashMap<&Address, f64> = FxHashMap::default();
    for leg in legs {
        *consumed_at
            .entry(leg.token_in)
            .or_insert(0.0) += leg.amount_in;
    }

    let mut ideal_at: FxHashMap<&Address, f64> = FxHashMap::default();
    ideal_at.insert(route.token_in, human_amount(route.amount_in, route.decimals_in)?);
    let mut ideal_out = 0.0_f64;

    for (token_in, collection) in branch_collections(legs, |leg| leg.token_in) {
        let available = ideal_at
            .get(&token_in)
            .copied()
            .ok_or_else(|| PriceImpactError::UnfedToken(token_in.clone()))?;
        let total = consumed_at
            .get(&token_in)
            .copied()
            .unwrap_or_default();
        if total <= 0.0 {
            return Err(PriceImpactError::NoFlow(token_in));
        }
        for leg in collection {
            if !(leg.spot.is_finite() && leg.spot > 0.0) {
                return Err(PriceImpactError::InvalidSpot {
                    token_in: leg.token_in.clone(),
                    token_out: leg.token_out.clone(),
                    spot: leg.spot,
                });
            }
            let contribution = available * (leg.amount_in / total) * leg.spot;
            if leg.token_out == route.token_out {
                ideal_out += contribution;
            } else {
                *ideal_at
                    .entry(leg.token_out)
                    .or_insert(0.0) += contribution;
            }
        }
    }

    if ideal_out.is_finite() && ideal_out > 0.0 {
        Ok(ideal_out)
    } else {
        Err(PriceImpactError::NoIdealOutput)
    }
}

fn human_amount(raw: &BigUint, decimals: u32) -> Result<f64, PriceImpactError> {
    let raw = raw
        .to_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| PriceImpactError::AmountOutOfRange(raw.clone()))?;
    Ok(raw / 10f64.powi(decimals as i32))
}

fn token_of<'v>(
    view: &'v MarketDataView<'_>,
    address: &Address,
) -> Result<&'v Token, PriceImpactError> {
    view.get_token(address)
        .ok_or_else(|| PriceImpactError::UnknownToken(address.clone()))
}

/// Price impact of a route from each swap's own spot price.
///
/// Each swap carries the component state its amounts were simulated against, so its spot price
/// is the pre-trade marginal price of that pool. The market view supplies only token decimals.
/// If a writer holds the market lock the quote goes out without a price impact rather than
/// waiting.
pub(crate) fn spot_price_impact(
    route: &Route,
    amount_in: &BigUint,
    amount_out: &BigUint,
    market: &MarketData,
) -> Result<f64, PriceImpactError> {
    let swaps = route.swaps();
    let (Some(first), Some(last)) = (swaps.first(), swaps.last()) else {
        return Err(PriceImpactError::EmptyRoute);
    };
    let view = market
        .try_read_blocking()
        .ok_or(PriceImpactError::MarketBusy)?;

    let mut legs = Vec::with_capacity(swaps.len());
    for swap in swaps {
        let spot = swap
            .protocol_state()
            .spot_price(token_of(&view, swap.token_in())?, token_of(&view, swap.token_out())?)
            .map_err(|err| PriceImpactError::SpotPrice {
                component_id: swap.component_id().to_string(),
                reason: err.to_string(),
            })?;
        let amount_in = swap
            .amount_in()
            .to_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| PriceImpactError::AmountOutOfRange(swap.amount_in().clone()))?;
        legs.push(SpotLeg {
            token_in: swap.token_in(),
            token_out: swap.token_out(),
            amount_in,
            spot,
        });
    }

    let endpoints = RouteEndpoints {
        token_in: first.token_in(),
        token_out: last.token_out(),
        amount_in,
        amount_out,
        decimals_in: token_of(&view, first.token_in())?.decimals,
        decimals_out: token_of(&view, last.token_out())?.decimals,
    };
    price_impact_from_spot_legs(&legs, &endpoints)
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            addr, component, setup_market_weighted, token, token_with_decimals, MockProtocolSim,
        },
        types::Swap,
    };

    fn bu(s: &str) -> BigUint {
        s.parse().unwrap()
    }

    fn endpoints<'a>(
        token_in: &'a Address,
        token_out: &'a Address,
        amount_in: &'a BigUint,
        amount_out: &'a BigUint,
    ) -> RouteEndpoints<'a> {
        RouteEndpoints {
            token_in,
            token_out,
            amount_in,
            amount_out,
            decimals_in: 0,
            decimals_out: 0,
        }
    }

    #[test]
    fn test_single_hop_near_one_to_one() {
        // 1_000_000 DAI (18dp) -> 999_843.73 USDC (6dp), spot 0.9998 USDC/DAI.
        let dai = addr(0x01);
        let usdc = addr(0x02);
        let legs = [SpotLeg { token_in: &dai, token_out: &usdc, amount_in: 1e24, spot: 0.9998 }];
        let (amount_in, amount_out) = (bu("1000000000000000000000000"), bu("999843730000"));
        let route = RouteEndpoints {
            decimals_in: 18,
            decimals_out: 6,
            ..endpoints(&dai, &usdc, &amount_in, &amount_out)
        };
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!(pi.abs() < 0.001, "expected ~0 impact, got {pi}");
    }

    #[test]
    fn test_linear_route_multiplies_spots() {
        // A->B at 2.0 then B->C at 0.5 is an ideal 1:1; 1000 in, 980 out => 2% impact.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [
            SpotLeg { token_in: &a, token_out: &b, amount_in: 1000.0, spot: 2.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in: 1990.0, spot: 0.5 },
        ];
        let (amount_in, amount_out) = (bu("1000"), bu("980"));
        let route = endpoints(&a, &c, &amount_in, &amount_out);
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!((pi - 0.02).abs() < 1e-9, "got {pi}");
    }

    #[test]
    fn test_favorable_execution() {
        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [SpotLeg { token_in: &a, token_out: &b, amount_in: 100.0, spot: 1.0 }];
        let (amount_in, amount_out) = (bu("100"), bu("101"));
        let route = endpoints(&a, &b, &amount_in, &amount_out);
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!(pi < 0.0, "got {pi}");
    }

    #[test]
    fn test_zero_spot() {
        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [SpotLeg { token_in: &a, token_out: &b, amount_in: 100.0, spot: 0.0 }];
        let (amount_in, amount_out) = (bu("100"), bu("100"));
        let route = endpoints(&a, &b, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &route).unwrap_err();
        assert!(matches!(err, PriceImpactError::InvalidSpot { spot, .. } if spot == 0.0), "{err}");
    }

    #[test]
    fn test_endpoint_decimals() {
        // 1 WETH (18dp) at spot 2000 USDC/WETH -> 2000 USDC (6dp): no impact.
        let weth = addr(0x01);
        let usdc = addr(0x02);
        let legs = [SpotLeg { token_in: &weth, token_out: &usdc, amount_in: 1e18, spot: 2000.0 }];
        let (amount_in, amount_out) = (bu("1000000000000000000"), bu("2000000000"));
        let route = RouteEndpoints {
            decimals_in: 18,
            decimals_out: 6,
            ..endpoints(&weth, &usdc, &amount_in, &amount_out)
        };
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!(pi.abs() < 1e-6, "got {pi}");
    }

    #[test]
    fn test_amount_beyond_f64() {
        // A 1e400 amount does not fit an f64; the walk must refuse it rather than divide by inf.
        let a = addr(0x01);
        let b = addr(0x02);
        let huge = "1".to_string() + &"0".repeat(400);
        let legs = [SpotLeg { token_in: &a, token_out: &b, amount_in: 1.0, spot: 1.0 }];
        let (amount_in, amount_out) = (bu(&huge), bu("1"));
        let route = endpoints(&a, &b, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &route).unwrap_err();
        assert!(matches!(err, PriceImpactError::AmountOutOfRange(_)), "{err}");
    }

    #[test]
    fn test_parallel_split_weights_branches_by_amount() {
        // 100 A split 60/40 across two A->B pools at spot 2.0 and 3.0: ideal 120 + 120 = 240,
        // actual 114 + 114 = 228 => 5% impact. A per-hop product cannot express this at all.
        let a = addr(0x01);
        let b = addr(0x02);
        let legs = [
            SpotLeg { token_in: &a, token_out: &b, amount_in: 60.0, spot: 2.0 },
            SpotLeg { token_in: &a, token_out: &b, amount_in: 40.0, spot: 3.0 },
        ];
        let (amount_in, amount_out) = (bu("100"), bu("228"));
        let route = endpoints(&a, &b, &amount_in, &amount_out);
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!((pi - 0.05).abs() < 1e-9, "got {pi}");
    }

    #[test]
    fn test_tree_split_compounds_intermediate_shortfall() {
        // 100 A: 50 straight to C at spot 2.0 (ideal 100), 50 to B at spot 1.0 (ideal 50, paid
        // 49), then B split 30/19 into two B->C pools at spot 2.0. The B collection is seeded
        // with the ideal 50, not the 49 paid, so the ideal at C is 100 + 100 = 200. Actual
        // 95 + 55 + 36 = 186 => 7% impact.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [
            SpotLeg { token_in: &a, token_out: &c, amount_in: 50.0, spot: 2.0 },
            SpotLeg { token_in: &a, token_out: &b, amount_in: 50.0, spot: 1.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in: 30.0, spot: 2.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in: 19.0, spot: 2.0 },
        ];
        let (amount_in, amount_out) = (bu("100"), bu("186"));
        let route = endpoints(&a, &c, &amount_in, &amount_out);
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!((pi - 0.07).abs() < 1e-9, "got {pi}");
    }

    #[test]
    fn test_ungrouped_swap_order() {
        // A valid route whose swaps are not grouped by input token: the second A swap is listed
        // after the C->D swap that consumes what it produces. Walking collection by collection,
        // C is seeded with both A->C and B->C (ideal 100) before C->D runs: ideal D = 200,
        // actual 190 => 5% impact. Walking in list order would seed C with 50 and report -90%.
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let d = addr(0x04);
        let legs = [
            SpotLeg { token_in: &a, token_out: &b, amount_in: 50.0, spot: 1.0 },
            SpotLeg { token_in: &b, token_out: &c, amount_in: 49.0, spot: 1.0 },
            SpotLeg { token_in: &c, token_out: &d, amount_in: 97.0, spot: 2.0 },
            SpotLeg { token_in: &a, token_out: &c, amount_in: 50.0, spot: 1.0 },
        ];
        let (amount_in, amount_out) = (bu("100"), bu("190"));
        let route = endpoints(&a, &d, &amount_in, &amount_out);
        let pi = price_impact_from_spot_legs(&legs, &route).unwrap();
        assert!((pi - 0.05).abs() < 1e-9, "got {pi}");
    }

    #[test]
    fn test_consumer_before_producer() {
        let a = addr(0x01);
        let b = addr(0x02);
        let c = addr(0x03);
        let legs = [SpotLeg { token_in: &b, token_out: &c, amount_in: 10.0, spot: 1.0 }];
        let (amount_in, amount_out) = (bu("10"), bu("10"));
        let route = endpoints(&a, &c, &amount_in, &amount_out);
        let err = price_impact_from_spot_legs(&legs, &route).unwrap_err();
        assert!(matches!(err, PriceImpactError::UnfedToken(ref token) if *token == b), "{err}");
    }

    /// A swap as a split route carries it: the pool's own state, from which the walk takes the
    /// spot price. `MockProtocolSim` quotes `spot` from the lower to the higher address.
    fn route_swap(
        pool_id: &str,
        token_in: &Token,
        token_out: &Token,
        amount_in: u64,
        amount_out: u64,
        spot: f64,
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
            Box::new(MockProtocolSim::new(spot)),
        )
        .with_split(split)
    }

    #[test]
    fn test_spot_price_impact_for_split_route() {
        // 100 A split 60/40 over two pools at spot 2.0, paying 114 and 78: 192 of an ideal 200.
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![
            ("p1", &a, &b, MockProtocolSim::new(2.0)),
            ("p2", &a, &b, MockProtocolSim::new(2.0)),
        ]);
        let route = Route::new(
            vec![
                route_swap("p1", &a, &b, 60, 114, 2.0, 0.6),
                route_swap("p2", &a, &b, 40, 78, 2.0, 0.0),
            ],
            FxHashMap::default(),
        )
        .unwrap();

        let pi = spot_price_impact(&route, &bu("100"), &bu("192"), &market).unwrap();
        assert!((pi - 0.04).abs() < 1e-9, "got {pi}");
    }

    #[test]
    fn test_spot_price_impact_reads_endpoint_decimals_from_market() {
        // 1 A (18dp) -> 2 B (6dp) at spot 2.0: the raw amounts differ by 1e12 but the impact is 0.
        let a = token_with_decimals(0x01, "A", 18);
        let b = token_with_decimals(0x02, "B", 6);
        let (market, _) = setup_market_weighted(vec![("p1", &a, &b, MockProtocolSim::new(2.0))]);
        let route = Route::new(
            vec![route_swap("p1", &a, &b, 1_000_000_000_000_000_000, 2_000_000, 2.0, 0.0)],
            FxHashMap::default(),
        )
        .unwrap();

        let pi =
            spot_price_impact(&route, &bu("1000000000000000000"), &bu("2000000"), &market).unwrap();
        assert!(pi.abs() < 1e-9, "got {pi}");
    }

    #[test]
    fn test_spot_price_impact_unknown_token() {
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let (market, _) = setup_market_weighted(vec![]);
        let route =
            Route::new(vec![route_swap("p1", &a, &b, 100, 200, 2.0, 0.0)], FxHashMap::default())
                .unwrap();

        let err = spot_price_impact(&route, &bu("100"), &bu("200"), &market).unwrap_err();
        assert!(
            matches!(err, PriceImpactError::UnknownToken(ref token) if *token == a.address),
            "{err}"
        );
    }
}
