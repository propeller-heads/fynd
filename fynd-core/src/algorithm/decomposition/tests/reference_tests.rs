//! Tests for the reference route.
//!
//! defibot has no test for `_build_reference_solution`; the cases here are built from the shapes
//! the Python enumerates (`order_solver.py:344-380`).

use num_bigint::BigUint;
use num_traits::Zero;
use rustc_hash::FxHashSet;
use tycho_simulation::tycho_core::{models::Address, simulation::protocol_sim::ProtocolSim};

use super::*;
use crate::{
    algorithm::{
        decomposition::{
            optimizers::pair_comparison::PairComparison,
            token_graph::{AllowedTokens, TokenGraph},
        },
        most_liquid::DepthAndPrice,
        test_utils::{market_read, setup_market_unweighted, token, MockProtocolSim},
    },
    feed::market_data::MarketData,
    graph::{
        petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
        GraphManager,
    },
};

/// `(component_id, token_in, token_out, spot_price)` for a fee-free mock pool.
type PoolSpec<'a> = (&'a str, Token, Token, f64);

fn market_with(
    pools: Vec<PoolSpec<'_>>,
) -> (MarketData, PetgraphStableDiGraphManager<DepthAndPrice>) {
    let pairs: Vec<(Token, Token)> = pools
        .iter()
        .map(|(_, token_in, token_out, _)| (token_in.clone(), token_out.clone()))
        .collect();
    let specs = pools
        .iter()
        .zip(&pairs)
        .map(|((id, _, _, spot_price), (token_in, token_out))| {
            let sim = MockProtocolSim::new(*spot_price)
                .with_fee(0.0)
                .with_tokens(&[token_in.clone(), token_out.clone()]);
            (*id, token_in, token_out, Box::new(sim) as Box<dyn ProtocolSim>)
        })
        .collect();
    let (market, _) = setup_market_unweighted(specs);
    // The shared helper builds an unweighted manager; a solve's graph carries edge weights.
    let mut manager = PetgraphStableDiGraphManager::<DepthAndPrice>::default();
    manager.initialize_graph(
        &market_read(&market)
            .base_market_state()
            .component_topology(),
    );
    (market, manager)
}

fn params<'a>(
    sell_token: &'a Token,
    buy_token: &'a Token,
    intermediate_token: &'a Token,
) -> ReferenceParams<'a> {
    ReferenceParams { sell_token, buy_token, intermediate_token, max_routes: 30, deadline: None }
}

/// Builds and solves the reference for a 1000-unit order at zero gas cost.
fn build(
    graph: &StableDiGraph<DepthAndPrice>,
    market: &MarketData,
    params: &ReferenceParams<'_>,
) -> Option<SolutionGraph> {
    build_for(graph, market, params, &BigUint::from(1_000u32))
}

fn build_for(
    graph: &StableDiGraph<DepthAndPrice>,
    market: &MarketData,
    params: &ReferenceParams<'_>,
    sell_amount: &BigUint,
) -> Option<SolutionGraph> {
    build_for_allowed(graph, market, params, sell_amount, None)
}

/// [`build_for`] with an allowlist of the tokens a route may pass through.
fn build_for_allowed(
    graph: &StableDiGraph<DepthAndPrice>,
    market: &MarketData,
    params: &ReferenceParams<'_>,
    sell_amount: &BigUint,
    connector_tokens: Option<&FxHashSet<Address>>,
) -> Option<SolutionGraph> {
    let view = market_read(market);
    let gas_price_wei = BigUint::zero();
    let graph = TokenGraph::new(
        graph,
        &AllowedTokens {
            connector_tokens,
            prices: None,
            endpoints: [&params.sell_token.address, &params.buy_token.address],
        },
    );
    build_reference_solution(
        &graph,
        view.base_market_state(),
        None,
        params,
        sell_amount,
        &PairComparison,
        &GasPrices::new(gas_price_wei.clone(), None),
    )
    .expect("the fixtures build")
}

/// Token sequence of every branch, rendered as `"A->W->B"`.
fn branch_labels(graph: &SolutionGraph) -> Vec<String> {
    graph
        .branches()
        .iter()
        .map(Branch::token_path_label)
        .collect()
}

#[test]
fn test_reference_is_the_direct_pool_plus_the_leg_through_the_intermediate() {
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let c = token(0x0C, "C");
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 2.0),
        ("aw", a.clone(), w.clone(), 3.0),
        ("wb", w.clone(), b.clone(), 4.0),
        // A path the reference must ignore: it goes through a token that is not the intermediate.
        ("ac", a.clone(), c.clone(), 5.0),
        ("cb", c.clone(), b.clone(), 5.0),
    ]);

    let reference = build(manager.graph(), &market, &params(&a, &b, &w)).expect("both legs exist");

    assert_eq!(branch_labels(&reference), ["A->B", "A->W->B"]);
}

#[test]
fn test_reference_is_solved_and_priced() {
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 2.0),
        ("aw", a.clone(), w.clone(), 3.0),
        ("wb", w.clone(), b.clone(), 4.0),
    ]);

    let reference = build(manager.graph(), &market, &params(&a, &b, &w)).expect("both legs exist");

    assert!(!reference.outer_splits().is_empty());
    assert!(!reference.buy_amount().is_zero());
    // The post-trade marginal price is what filters candidate paths, so it has to be available.
    assert!(reference.new_marginal_price().is_some());
}

#[test]
fn test_reference_without_a_post_trade_price_is_dropped() {
    // `order_solver.py:228-235`. Nothing was traded, so no pool has a post-trade state and the
    // reference cannot state the price it exists to provide. Keeping it would hand the caller a
    // filter floor and a comparison baseline built on a price that is not there.
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let (market, manager) = market_with(vec![("ab", a.clone(), b.clone(), 2.0)]);

    let reference = build_for(manager.graph(), &market, &params(&a, &b, &w), &BigUint::zero());

    assert!(reference.is_none());
}

#[test]
fn test_reference_falls_back_to_the_two_hop_leg_without_a_direct_pool() {
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let (market, manager) =
        market_with(vec![("aw", a.clone(), w.clone(), 3.0), ("wb", w.clone(), b.clone(), 4.0)]);

    let reference = build(manager.graph(), &market, &params(&a, &b, &w)).expect("one leg exists");

    assert_eq!(branch_labels(&reference), ["A->W->B"]);
}

#[test]
fn test_reference_is_the_direct_pool_alone_when_the_intermediate_leg_is_missing() {
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let (market, manager) =
        market_with(vec![("ab", a.clone(), b.clone(), 2.0), ("aw", a.clone(), w.clone(), 3.0)]);

    let reference = build(manager.graph(), &market, &params(&a, &b, &w)).expect("one leg exists");

    assert_eq!(branch_labels(&reference), ["A->B"]);
}

#[test]
fn test_reference_is_none_when_neither_leg_exists() {
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let c = token(0x0C, "C");
    // A and B are connected, but only through a token that is not the intermediate one.
    let (market, manager) =
        market_with(vec![("ac", a.clone(), c.clone(), 2.0), ("cb", c.clone(), b.clone(), 2.0)]);

    assert!(build(manager.graph(), &market, &params(&a, &b, &w)).is_none());
}

#[test]
fn test_reference_with_the_intermediate_as_an_endpoint_skips_the_two_hop_leg() {
    // Selling the intermediate token itself: the two-hop leg would revisit an endpoint, so defibot
    // asks for a plain depth-2 subgraph instead (`order_solver.py:377-380`).
    let (b, w) = (token(0x0B, "B"), token(0x0F, "W"));
    let c = token(0x0C, "C");
    let (market, manager) = market_with(vec![
        ("wb", w.clone(), b.clone(), 4.0),
        ("wc", w.clone(), c.clone(), 2.0),
        ("cb", c.clone(), b.clone(), 2.0),
    ]);

    let reference =
        build(manager.graph(), &market, &params(&w, &b, &w)).expect("the direct pool exists");

    assert!(branch_labels(&reference).contains(&"W->B".to_string()));
}

#[test]
fn test_reference_honours_the_token_allowlist() {
    let (a, b, w) = (token(0x0A, "A"), token(0x0B, "B"), token(0x0F, "W"));
    let (market, manager) = market_with(vec![
        ("ab", a.clone(), b.clone(), 2.0),
        ("aw", a.clone(), w.clone(), 3.0),
        ("wb", w.clone(), b.clone(), 4.0),
    ]);
    // `W` is the intermediate the reference would use, and the allowlist keeps it out.
    let allowed = FxHashSet::from_iter([a.address.clone(), b.address.clone()]);

    let reference = build_for_allowed(
        manager.graph(),
        &market,
        &params(&a, &b, &w),
        &BigUint::from(1_000u32),
        Some(&allowed),
    )
    .expect("the direct pool");

    assert_eq!(branch_labels(&reference), ["A->B"]);
}
