//! Tests for the reference route.
//!
//! defibot has no test for `_build_reference_solution`; the cases here are built from the shapes
//! the Python enumerates (`order_solver.py:344-380`).
//!
//! The reference no longer searches for its own paths — `DecompositionAlgorithm::search_graph`
//! enumerates them alongside the candidate paths and hands them over. So what is tested here is
//! only what the function still decides: whether the paths it was given produce a graph, whether
//! that graph solves, and whether it can state a post-trade price. Which paths reach it is decided
//! in `mod.rs` and covered by `algorithm_tests`.

use std::time::Duration;

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::tycho_core::{
    models::{token::Token, Address},
    simulation::protocol_sim::ProtocolSim,
};

use super::*;
use crate::{
    algorithm::{
        decomposition::{
            components::Route,
            models::DirectPath,
            optimizers::SplitOptimizerConfig,
            token_graph::{AllowedTokens, TokenGraph},
            SolveRequest,
        },
        most_liquid::DepthAndPrice,
        test_utils::{market_read, setup_market_unweighted, token, MockProtocolSim},
    },
    feed::market_data::MarketData,
    graph::{
        petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
        GraphManager,
    },
    types::{Order, OrderSide},
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

/// One path, named by the tokens it visits and the component traded at each hop.
fn path(tokens: &[&Token], components: &[&str]) -> DirectPath {
    DirectPath {
        tokens: tokens
            .iter()
            .map(|token| token.address.clone())
            .collect(),
        components: components
            .iter()
            .map(|id| (*id).to_string())
            .collect(),
    }
}

fn order(sell_token: &Token, buy_token: &Token, amount: u32) -> Order {
    Order::new(
        sell_token.address.clone(),
        buy_token.address.clone(),
        BigUint::from(amount),
        OrderSide::Sell,
        Address::from([0u8; 20]),
    )
}

/// Builds and solves the reference over `paths` at zero gas cost.
fn build(
    graph: &StableDiGraph<DepthAndPrice>,
    market: &MarketData,
    order: &Order,
    paths: Vec<DirectPath>,
) -> Option<DecompositionGraph> {
    let view = market_read(market);
    let token_graph = TokenGraph::new(
        graph,
        &AllowedTokens {
            connector_tokens: None,
            prices: None,
            endpoints: [order.token_in(), order.token_out()],
        },
    );
    let input = SolveRequest::new(order, token_graph, None, SplitOptimizerConfig::default());
    reference_algorithm().solve_reference_solution(
        &input,
        paths,
        view.base_market_state(),
        &TokenPriceData::new(BigUint::zero(), None),
    )
}

/// An algorithm whose only relevant setting is the parallel-alternative cap the reference inherits.
fn reference_algorithm() -> DecompositionAlgorithm {
    let config = AlgorithmConfig::new(1, 3, Duration::from_millis(100), None)
        .expect("valid algorithm config");
    let decomposition = DecompositionConfig { max_parallel_routes: 30, ..Default::default() };
    DecompositionAlgorithm::new(config, decomposition).expect("valid decomposition config")
}

/// Token sequence of every branch, rendered as `"A->W->B"`.
fn branch_labels(graph: &DecompositionGraph) -> Vec<String> {
    graph
        .inner()
        .iter()
        .map(Route::token_path_label)
        .collect()
}

/// The direct pool and the route through the intermediate become one branch each.
#[test]
fn test_reference_keeps_every_path_it_is_given() {
    let (a, w, b) = (token(0x0A, "A"), token(0x0C, "W"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![
        ("direct", a.clone(), b.clone(), 2.0),
        ("a_w", a.clone(), w.clone(), 2.0),
        ("w_b", w.clone(), b.clone(), 2.0),
    ]);

    let reference = build(
        manager.graph(),
        &market,
        &order(&a, &b, 1_000),
        vec![path(&[&a, &b], &["direct"]), path(&[&a, &w, &b], &["a_w", "w_b"])],
    )
    .expect("both paths price");

    let labels = branch_labels(&reference);
    assert_eq!(labels.len(), 2, "one branch per path, got {labels:?}");
}

/// A solved reference carries splits and a post-trade price, which is what makes it usable as the
/// candidate filter's floor.
#[test]
fn test_reference_is_solved_and_priced() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("direct", a.clone(), b.clone(), 2.0)]);

    let reference =
        build(manager.graph(), &market, &order(&a, &b, 1_000), vec![path(&[&a, &b], &["direct"])])
            .expect("the direct pool prices");

    assert!(reference.solved(), "the reference is returned solved");
    assert!(reference.new_marginal_price().is_some(), "and able to state its post-trade price");
    assert!(!reference.buy_amount().is_zero(), "and to say what it bought");
}

/// No paths means no reference, and that is not an error — a thinly connected pair legitimately has
/// neither a direct pool nor a route through a connector, and the caller then solves without one.
#[test]
fn test_reference_is_none_without_paths() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("direct", a.clone(), b.clone(), 2.0)]);

    let reference = build(manager.graph(), &market, &order(&a, &b, 1_000), Vec::new());

    assert!(reference.is_none());
}

/// A path naming a component the market does not hold prices nothing, so the reference is dropped
/// rather than built from an empty graph.
#[test]
fn test_reference_is_none_when_no_path_prices() {
    let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
    let (market, manager) = market_with(vec![("direct", a.clone(), b.clone(), 2.0)]);

    let reference =
        build(manager.graph(), &market, &order(&a, &b, 1_000), vec![path(&[&a, &b], &["ghost"])]);

    assert!(reference.is_none());
}
