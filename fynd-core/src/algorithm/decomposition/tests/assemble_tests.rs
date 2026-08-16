//! Tests for turning a solved [`DecompositionGraph`] into an encodable route.

use std::sync::Arc;

use num_bigint::{BigInt, BigUint};
use rustc_hash::FxHashMap;
use tycho_simulation::tycho_core::{
    models::token::Token,
    simulation::protocol_sim::{Price, ProtocolSim},
};

use super::*;
use crate::{
    algorithm::{
        decomposition::{
            components::Fraction,
            test_fixtures::{
                graph, hop, route, single_pool_hop, solved_hop, split, tenfold_pool, token_a,
                token_b, token_c, FixedRateSim,
            },
        },
        test_utils::{component, order},
    },
    derived::types::TokenGasPrices,
    types::OrderSide,
};

/// A market holding one `FixedRateSim` pool per `(id, token_in, token_out)`, paying `multiple`.
fn market(pools: &[(&str, Token, Token, u64)]) -> MarketState {
    let mut market = MarketState::new();
    for (id, token_in, token_out, multiple) in pools {
        let tokens = vec![token_in.clone(), token_out.clone()];
        market.upsert_components(std::iter::once(component(id, &tokens)));
        market.update_states([(
            (*id).to_string(),
            Box::new(FixedRateSim::new(*multiple)) as Box<dyn ProtocolSim>,
        )]);
        market.upsert_tokens(tokens);
    }
    market
}

/// A market where one named pool stops simulating above `threshold`, the way a concentrated
/// liquidity pool answers `Ticks exceeded` once a swap would cross more ticks than it holds.
fn market_with_brittle_pool(
    pools: &[(&str, Token, Token, u64)],
    brittle: &str,
    threshold: u64,
) -> MarketState {
    let mut market = MarketState::new();
    for (id, token_in, token_out, multiple) in pools {
        let tokens = vec![token_in.clone(), token_out.clone()];
        let mut sim = FixedRateSim::new(*multiple);
        if *id == brittle {
            sim = sim.with_simulation_failure_above(threshold);
        }
        market.upsert_components(std::iter::once(component(id, &tokens)));
        market.update_states([((*id).to_string(), Box::new(sim) as Box<dyn ProtocolSim>)]);
        market.upsert_tokens(tokens);
    }
    market
}

/// Amounts leaving the order's input token, summed over the route's root swaps.
fn routed_input(result: &RouteResult, order: &Order) -> BigUint {
    result
        .route()
        .swaps()
        .iter()
        .filter(|swap| swap.token_in() == order.token_in())
        .map(|swap| swap.amount_in().clone())
        .sum()
}

/// Output the route produces, summed over the swaps reaching the order's output token.
fn produced_output(result: &RouteResult, order: &Order) -> BigUint {
    result
        .route()
        .swaps()
        .iter()
        .filter(|swap| swap.token_out() == order.token_out())
        .map(|swap| swap.amount_out().clone())
        .sum()
}

#[test]
fn test_parallel_pools_expand_into_one_allocation_each() {
    let solution = graph(
        vec![route(
            vec![token_a(), token_b()],
            vec![solved_hop(
                token_a(),
                token_b(),
                vec![tenfold_pool("p1"), tenfold_pool("p2")],
                vec![split(1, 2); 2],
            )],
        )],
        vec![Fraction::one()],
    );

    let allocations = solution_allocations(&solution);

    assert_eq!(allocations.len(), 2);
    for allocation in &allocations {
        assert_eq!(allocation.hops.len(), 1);
        assert!((allocation.flow_fraction - 0.5).abs() < 1e-12);
    }
}

#[test]
fn test_multi_hop_branch_expands_into_the_product_of_its_legs() {
    // Two pools at each of two legs is four linear paths, each carrying a quarter of the branch.
    let solution = graph(
        vec![route(
            vec![token_a(), token_b(), token_c()],
            vec![
                solved_hop(
                    token_a(),
                    token_b(),
                    vec![tenfold_pool("ab1"), tenfold_pool("ab2")],
                    vec![split(1, 2); 2],
                ),
                solved_hop(
                    token_b(),
                    token_c(),
                    vec![tenfold_pool("bc1"), tenfold_pool("bc2")],
                    vec![split(1, 2); 2],
                ),
            ],
        )],
        vec![Fraction::one()],
    );

    let allocations = solution_allocations(&solution);

    assert_eq!(allocations.len(), 4);
    let total: f64 = allocations
        .iter()
        .map(|allocation| allocation.flow_fraction)
        .sum();
    assert!((total - 1.0).abs() < 1e-12, "flows must still cover the whole order, got {total}");
    assert!(allocations
        .iter()
        .all(|allocation| allocation.hops.len() == 2));
}

#[test]
fn test_zero_split_pools_and_branches_are_dropped() {
    let solution = graph(
        vec![
            route(
                vec![token_a(), token_b()],
                vec![solved_hop(
                    token_a(),
                    token_b(),
                    vec![tenfold_pool("kept"), tenfold_pool("dropped")],
                    vec![Fraction::one(), Fraction::zero()],
                )],
            ),
            route(
                vec![token_a(), token_b()],
                vec![single_pool_hop(token_a(), token_b(), tenfold_pool("idle_branch"))],
            ),
        ],
        vec![Fraction::one(), Fraction::zero()],
    );

    let allocations = solution_allocations(&solution);

    assert_eq!(allocations.len(), 1);
    assert_eq!(
        allocations[0].hops[0]
            .descriptor
            .component_id,
        "kept"
    );
}

#[test]
fn test_unsolved_hop_drops_its_whole_branch() {
    // A branch with a dead leg carries no flow; emitting its earlier legs would strand tokens.
    let solution = graph(
        vec![route(
            vec![token_a(), token_b(), token_c()],
            vec![
                single_pool_hop(token_a(), token_b(), tenfold_pool("ab")),
                hop(token_b(), token_c(), vec![tenfold_pool("bc")]),
            ],
        )],
        vec![Fraction::one()],
    );

    assert!(solution_allocations(&solution).is_empty());
}

#[test]
fn test_route_result_spends_the_whole_order_despite_split_rounding() {
    // Three pools on `floor(amount / 3)` each leaves one unit unrouted inside the solver
    // (`test_hop_sell_loses_up_to_one_unit_per_pool_to_rounding`). The assembled route must still
    // spend every unit of the order, because the last leg's `split = 0.0` takes the remainder.
    let amount = 100u128;
    let order = order(&token_a(), &token_b(), amount, OrderSide::Sell);
    let market = market(&[
        ("p1", token_a(), token_b(), 10),
        ("p2", token_a(), token_b(), 10),
        ("p3", token_a(), token_b(), 10),
    ]);
    let mut solution = graph(
        vec![route(
            vec![token_a(), token_b()],
            vec![solved_hop(
                token_a(),
                token_b(),
                vec![tenfold_pool("p1"), tenfold_pool("p2"), tenfold_pool("p3")],
                vec![split(1, 3); 3],
            )],
        )],
        vec![Fraction::one()],
    );
    let (solver_bought, _) = solution
        .sell(&BigUint::from(amount))
        .expect("the solver sells");

    let gas_price_wei = BigUint::from(1u8);
    let gas_prices = TokenPriceData::new(gas_price_wei.clone(), None);
    let result =
        cast_into_route(&solution, &market, &order, &gas_prices).expect("the solution assembles");

    // The solver routed 99 of the 100 and bought 990; the route routes all 100 and buys 1000.
    assert_eq!(solver_bought, BigUint::from(990u32));
    assert_eq!(routed_input(&result, &order), BigUint::from(amount));
    assert_eq!(produced_output(&result, &order), BigUint::from(1_000u32));
    // The quote is the route's own number, not the solver's.
    assert_eq!(result.net_amount_out(), &BigInt::from(1_000u32));
}

#[test]
fn test_route_result_charges_gas_in_buy_token_units() {
    let order = order(&token_a(), &token_b(), 100, OrderSide::Sell);
    let market = market(&[("p1", token_a(), token_b(), 10)]);
    let solution = graph(
        vec![route(
            vec![token_a(), token_b()],
            vec![single_pool_hop(token_a(), token_b(), tenfold_pool("p1"))],
        )],
        vec![Fraction::one()],
    );

    let gas_price_wei = BigUint::from(2u8);
    let mut token_prices: TokenGasPrices = FxHashMap::default();
    token_prices.insert(token_b().address, Price::new(BigUint::from(3u8), BigUint::from(1u8)));
    let gas_prices =
        TokenPriceData::new(gas_price_wei.clone(), Some(Arc::new(token_prices.clone())));

    let result =
        cast_into_route(&solution, &market, &order, &gas_prices).expect("the solution assembles");

    // `FixedRateSim` reports one gas unit: 1 gas * 2 wei/gas * 3 token-per-wei = 6.
    assert_eq!(produced_output(&result, &order), BigUint::from(1_000u32));
    assert_eq!(result.net_amount_out(), &BigInt::from(994u32));
}

#[test]
fn test_shortfall_solution_is_stretched_and_requoted_at_the_full_amount() {
    // The solver only allocated a twentieth of the order. An exact-in route spends all of it, so
    // `build_split_route` renormalises — and re-simulates, so the quote describes the stretched
    // route rather than extrapolating the twentieth. Refusing instead discarded candidates worth
    // several times the reference on the recorded fixture.
    let order = order(&token_a(), &token_b(), 10_000, OrderSide::Sell);
    let market = market(&[("p1", token_a(), token_b(), 10), ("p2", token_a(), token_b(), 10)]);
    let solution = graph(
        vec![
            route(
                vec![token_a(), token_b()],
                vec![single_pool_hop(token_a(), token_b(), tenfold_pool("p1"))],
            ),
            route(
                vec![token_a(), token_b()],
                vec![single_pool_hop(token_a(), token_b(), tenfold_pool("p2"))],
            ),
        ],
        vec![split(1, 20), Fraction::zero()],
    );

    let gas_price_wei = BigUint::from(1u8);
    let gas_prices = TokenPriceData::new(gas_price_wei.clone(), None);
    let result = cast_into_route(&solution, &market, &order, &gas_prices)
        .expect("a shortfall solution assembles at the full order amount");

    // The whole order goes through `p1` — the only branch carrying flow — at its tenfold rate,
    // which is the full 10_000 in rather than the 500 the solver sized it for.
    assert_eq!(produced_output(&result, &order), BigUint::from(100_000u32));
}

/// Reproduces the GNO cluster on the recorded fixture, where every failure was one pool answering
/// `Invalid input: Ticks exceeded` (`0xf7878463070a013a58f547b2b08df47a1fb91744`).
///
/// The solver allocated a small fraction of the order — `routed_flow` between 0.07 and 0.20 —
/// so stretching to the full amount multiplies every branch by five to fourteen. A concentrated
/// liquidity pool sized for the fraction cannot serve that, and because assembly simulates the
/// whole solution as one plan, the single failing pool takes the other branches down with it. The
/// caller then falls back to a reference route worth a fraction of the discarded candidate.
#[test]
fn test_one_unservable_pool_discards_the_whole_stretched_solution() {
    let order = order(&token_a(), &token_b(), 10_000, OrderSide::Sell);
    let market = market_with_brittle_pool(
        &[("healthy", token_a(), token_b(), 10), ("brittle", token_a(), token_b(), 10)],
        "brittle",
        1_000,
    );
    // Two branches each carrying a tenth of the order: `routed_flow` is 0.2, so assembly stretches
    // both by five and the brittle pool sees 5_000 against a 1_000 ceiling.
    let solution = graph(
        vec![
            route(
                vec![token_a(), token_b()],
                vec![single_pool_hop(token_a(), token_b(), tenfold_pool("healthy"))],
            ),
            route(
                vec![token_a(), token_b()],
                vec![single_pool_hop(token_a(), token_b(), tenfold_pool("brittle"))],
            ),
        ],
        vec![split(1, 10), split(1, 10)],
    );

    let gas_price_wei = BigUint::from(1u8);
    let gas_prices = TokenPriceData::new(gas_price_wei.clone(), None);
    let error = cast_into_route(&solution, &market, &order, &gas_prices)
        .expect_err("the brittle pool cannot serve its stretched share");

    // The whole solution is lost, including the healthy branch that would have served 5_000 fine.
    assert!(
        matches!(&error, AlgorithmError::SimulationFailed { component_id, .. }
            if component_id == "brittle"),
        "got {error:?}"
    );
}

/// The other half of the reproduction: the healthy branch alone assembles at the full order.
///
/// Nothing about the market prevents a good answer here — only that assembly is all-or-nothing.
/// Dropping the branch whose pool failed and re-assembling would return this instead of falling
/// back to the reference.
#[test]
fn test_the_healthy_branch_alone_assembles_at_the_full_order() {
    let order = order(&token_a(), &token_b(), 10_000, OrderSide::Sell);
    let market = market_with_brittle_pool(
        &[("healthy", token_a(), token_b(), 10), ("brittle", token_a(), token_b(), 10)],
        "brittle",
        1_000,
    );
    let solution = graph(
        vec![route(
            vec![token_a(), token_b()],
            vec![single_pool_hop(token_a(), token_b(), tenfold_pool("healthy"))],
        )],
        vec![split(1, 10)],
    );

    let gas_price_wei = BigUint::from(1u8);
    let gas_prices = TokenPriceData::new(gas_price_wei.clone(), None);
    let result = cast_into_route(&solution, &market, &order, &gas_prices)
        .expect("the healthy pool serves the whole order");

    assert_eq!(produced_output(&result, &order), BigUint::from(100_000u32));
}

#[test]
fn test_solution_activating_nothing_is_insufficient_liquidity() {
    let order = order(&token_a(), &token_b(), 100, OrderSide::Sell);
    let market = market(&[("p1", token_a(), token_b(), 10)]);
    let solution = graph(
        vec![route(
            vec![token_a(), token_b()],
            vec![single_pool_hop(token_a(), token_b(), tenfold_pool("p1"))],
        )],
        vec![Fraction::zero()],
    );

    let gas_price_wei = BigUint::from(1u8);
    let gas_prices = TokenPriceData::new(gas_price_wei.clone(), None);
    let error = cast_into_route(&solution, &market, &order, &gas_prices)
        .expect_err("a solution routing nothing has no route");

    assert!(matches!(error, AlgorithmError::InsufficientLiquidity), "got {error:?}");
}
