//! End-to-end tests through [`DecompositionAlgorithm::find_best_route`].

use num_bigint::{BigInt, BigUint};
use rustc_hash::FxHashSet;
use tycho_simulation::tycho_core::simulation::protocol_sim::ProtocolSim;

use super::*;
use crate::{
    algorithm::{
        decomposition::optimizers::SplitOptimizer,
        test_utils::{
            market_read, order, setup_market_weighted_petgraph, token, ConstantProductSim,
        },
    },
    graph::GraphManager,
    replay::replay_route,
    types::OrderSide,
    NoPathReason,
};

/// An `xy=k` pool. Reserves are given in address order, the convention `ConstantProductSim` stores
/// them in.
fn pool(reserve_0: u64, reserve_1: u64) -> Box<dyn ProtocolSim> {
    Box::new(ConstantProductSim {
        reserve_0: BigUint::from(reserve_0),
        reserve_1: BigUint::from(reserve_1),
        gas: 50_000,
    })
}

fn tokens() -> (Token, Token, Token, Token) {
    (token(0x0A, "A"), token(0x0B, "B"), token(0x0C, "C"), token(0x0D, "D"))
}

/// A `DecompositionAlgorithm` over `AlgorithmConfig::new(1, max_hops, 100ms, max_routes)`.
fn algorithm(
    max_hops: usize,
    max_routes: Option<usize>,
    decomposition: DecompositionConfig,
) -> DecompositionAlgorithm {
    let config = AlgorithmConfig::new(1, max_hops, Duration::from_millis(100), max_routes)
        .expect("valid algorithm config");
    DecompositionAlgorithm::new(config, decomposition).expect("valid decomposition config")
}

/// Components the returned route swaps on.
fn route_components(result: &RouteResult) -> Vec<&str> {
    result
        .route()
        .swaps()
        .iter()
        .map(|swap| swap.component_id())
        .collect()
}

/// Input the route spends out of the order's sell token.
fn routed_input(result: &RouteResult, order: &Order) -> BigUint {
    result
        .route()
        .swaps()
        .iter()
        .filter(|swap| swap.token_in() == order.token_in())
        .map(|swap| swap.amount_in().clone())
        .sum()
}

/// Output the route produces in the order's buy token.
fn produced_output(result: &RouteResult, order: &Order) -> BigUint {
    result
        .route()
        .swaps()
        .iter()
        .filter(|swap| swap.token_out() == order.token_out())
        .map(|swap| swap.amount_out().clone())
        .sum()
}

#[tokio::test]
async fn test_splits_an_order_across_parallel_pools() {
    let (a, b, _, _) = tokens();
    // Two identical pools, and an order large enough that price impact makes splitting pay.
    let (market, manager) = setup_market_weighted_petgraph(vec![
        ("p1", &a, &b, pool(1_000_000, 1_000_000)),
        ("p2", &a, &b, pool(1_000_000, 1_000_000)),
    ]);
    let order = order(&a, &b, 400_000, OrderSide::Sell);

    let result = algorithm(1, None, DecompositionConfig::default())
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect("the order routes");

    let components = route_components(&result);
    assert_eq!(components.len(), 2, "expected a split, got {components:?}");
    assert!(components.contains(&"p1") && components.contains(&"p2"), "got {components:?}");
    // One pool alone returns 400_000 * 1e6 / 1.4e6 = 285_714. Any real split beats that.
    assert!(
        produced_output(&result, &order) > BigUint::from(285_714u32),
        "a split must beat the single pool, got {}",
        produced_output(&result, &order)
    );
}

#[tokio::test]
async fn test_equal_start_v2_splits_an_order_across_parallel_pools() {
    // The same market as `test_splits_an_order_across_parallel_pools`, solved by the other
    // optimizer. Two identical pools have an exactly even optimum, so both must find it.
    let (a, b, _, _) = tokens();
    let (market, manager) = setup_market_weighted_petgraph(vec![
        ("p1", &a, &b, pool(1_000_000, 1_000_000)),
        ("p2", &a, &b, pool(1_000_000, 1_000_000)),
    ]);
    let order = order(&a, &b, 400_000, OrderSide::Sell);
    let config = DecompositionConfig::default().with_optimizers(SplitOptimizerConfig {
        outer: SplitOptimizer::EqualStartV2,
        inner: SplitOptimizer::EqualStartV2,
    });

    let result = algorithm(1, None, config)
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect("the order routes");

    let components = route_components(&result);
    assert_eq!(components.len(), 2, "expected a split, got {components:?}");
    assert!(
        produced_output(&result, &order) > BigUint::from(285_714u32),
        "a split must beat the single pool, got {}",
        produced_output(&result, &order)
    );
}

#[tokio::test]
async fn test_candidate_beats_the_reference_on_a_path_the_reference_cannot_see() {
    let (a, b, c, d) = tokens();
    // The reference is direct pools plus the leg through the connector `C`. The deep route runs
    // through `D`, which only the full candidate subgraph enumerates.
    let (market, manager) = setup_market_weighted_petgraph(vec![
        ("ab", &a, &b, pool(100_000, 100_000)),
        ("ac", &a, &c, pool(100_000, 100_000)),
        ("cb", &c, &b, pool(100_000, 100_000)),
        ("ad", &a, &d, pool(10_000_000, 10_000_000)),
        ("db", &d, &b, pool(10_000_000, 10_000_000)),
    ]);
    let order = order(&a, &b, 50_000, OrderSide::Sell);
    let config = DecompositionConfig::default().with_connector_token(c.address.clone());

    let result = algorithm(2, None, config)
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect("the order routes");

    let components = route_components(&result);
    assert!(
        components.contains(&"ad") && components.contains(&"db"),
        "the candidate should have won on the deep route through D, got {components:?}"
    );
}

#[tokio::test]
async fn test_reference_wins_when_the_candidate_ranks_a_worse_branch_first() {
    let (a, b, c, _) = tokens();
    // Candidate ranking is `inertia * (1 - fee) * price` and no depths are supplied, so it reduces
    // to price. A->C->B quotes 1.2 * 1.2 = 1.44 while the direct pool quotes 1.3, so with
    // `max_routes = 1` the candidate keeps only the two-hop path — better priced at zero size, and
    // shallow enough that at this order size it realises less. The reference keeps the deep direct
    // pool as well and wins on output net of gas.
    let (market, manager) = setup_market_weighted_petgraph(vec![
        // A < B, so reserve_0 is A: spot(A->B) = 13_000_000 / 10_000_000 = 1.3.
        ("ab", &a, &b, pool(10_000_000, 13_000_000)),
        // A < C, so reserve_0 is A: spot(A->C) = 120_000 / 100_000 = 1.2.
        ("ac", &a, &c, pool(100_000, 120_000)),
        // B < C, so reserve_0 is B: spot(C->B) = 144_000 / 120_000 = 1.2.
        ("cb", &c, &b, pool(144_000, 120_000)),
    ]);
    let order = order(&a, &b, 40_000, OrderSide::Sell);
    let config = DecompositionConfig::default().with_connector_token(c.address.clone());

    let result = algorithm(2, Some(1), config)
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect("the order routes");

    let components = route_components(&result);
    assert!(
        components.contains(&"ab"),
        "the reference's deep direct pool should have won, got {components:?}"
    );
    assert_eq!(routed_input(&result, &order), BigUint::from(40_000u32));
}

#[tokio::test]
async fn test_order_with_no_connecting_path_is_rejected() {
    let (a, b, c, d) = tokens();
    // A/B and C/D are two disconnected components of the graph.
    let (market, manager) = setup_market_weighted_petgraph(vec![
        ("ab", &a, &b, pool(1_000_000, 1_000_000)),
        ("cd", &c, &d, pool(1_000_000, 1_000_000)),
    ]);
    let order = order(&a, &d, 1_000, OrderSide::Sell);

    let error = algorithm(3, None, DecompositionConfig::default())
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect_err("nothing can fill this order");

    assert!(
        matches!(error, AlgorithmError::NoPath { reason: NoPathReason::NoGraphPath, .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn test_timeout_still_returns_a_complete_reference_route() {
    let (a, b, c, d) = tokens();
    let (market, manager) = setup_market_weighted_petgraph(vec![
        ("ab", &a, &b, pool(1_000_000, 1_000_000)),
        ("ac", &a, &c, pool(1_000_000, 1_000_000)),
        ("cb", &c, &b, pool(1_000_000, 1_000_000)),
        ("ad", &a, &d, pool(1_000_000, 1_000_000)),
        ("db", &d, &b, pool(1_000_000, 1_000_000)),
    ]);
    let order = order(&a, &b, 100_000, OrderSide::Sell);
    let config = AlgorithmConfig::new(1, 3, Duration::ZERO, None).expect("valid algorithm config");
    let decomposition = DecompositionConfig::default().with_connector_token(c.address.clone());
    let algorithm =
        DecompositionAlgorithm::new(config, decomposition).expect("valid decomposition config");

    // The deadline is already in the past when the candidate stage is reached, so the candidate
    // subgraph is skipped entirely. Path enumeration itself reads the clock only every
    // `DEADLINE_CHECK_INTERVAL` edges, which this graph never reaches — that bounded overshoot is
    // the deliberate trade in `graph_build`, and it is what leaves the reference buildable.
    let result = algorithm
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect("a timed-out solve still returns the reference");

    result
        .route()
        .validate()
        .expect("a timed-out solve must not return a partial route");
    assert_eq!(routed_input(&result, &order), BigUint::from(100_000u32));
    // Only the candidate subgraph reaches D; the reference is direct pools plus the leg through C.
    let components = route_components(&result);
    assert!(
        !components.contains(&"ad") && !components.contains(&"db"),
        "the candidate stage should have been skipped, got {components:?}"
    );
}

#[tokio::test]
async fn test_quoted_amounts_match_the_route_that_would_be_encoded() {
    let (a, b, c, d) = tokens();
    // Deliberately awkward reserves: parallel pools at the first leg and prime-ish sizes, so the
    // solver's per-pool flooring leaves a remainder the encoder has to absorb.
    let (market, manager) = setup_market_weighted_petgraph(vec![
        ("ab1", &a, &b, pool(1_000_001, 999_997)),
        ("ab2", &a, &b, pool(1_000_003, 999_991)),
        ("ac", &a, &c, pool(2_000_003, 1_999_997)),
        ("cb", &c, &b, pool(3_000_001, 2_999_993)),
        ("ad", &a, &d, pool(1_500_007, 1_499_989)),
        ("db", &d, &b, pool(1_500_011, 1_499_983)),
    ]);
    let order = order(&a, &b, 333_333, OrderSide::Sell);
    let config = DecompositionConfig::default().with_connector_token(c.address.clone());

    let result = algorithm(2, None, config)
        .find_best_route(manager.graph(), market.clone(), None, None, &order)
        .await
        .expect("the order routes");

    // Every unit of the order is spent: nothing is left behind by the solver's flooring.
    assert_eq!(
        routed_input(&result, &order),
        BigUint::from(333_333u32),
        "the assembled route must spend the whole order"
    );
    // The quote is what re-executing the route against the same market produces, exactly. With no
    // token gas prices the gas charge is zero, so the net amount is the gross output.
    let view = market_read(&market);
    let replay = replay_route(result.route(), view.base_market_state()).expect("the route replays");
    assert_eq!(replay.amount_out, produced_output(&result, &order));
    assert_eq!(result.net_amount_out(), &BigInt::from(replay.amount_out.clone()));
}

/// A market where C is the highest-degree token and D is a second, shallower connector.
///
/// ```text
///   A --- C --- B        C also touches E, so its degree beats D's.
///    \_ D _/
/// ```
/// There is deliberately no direct A/B pool: a reference route can only exist by going through one
/// of the two connectors, so which one was chosen is visible in the returned swaps.
fn two_connector_market() -> (MarketData, PetgraphStableDiGraphManager<DepthAndPrice>) {
    let (a, b, c, d) = tokens();
    let e = token(0x0E, "E");
    setup_market_weighted_petgraph(vec![
        ("ac", &a, &c, pool(1_000_000, 1_000_000)),
        ("cb", &c, &b, pool(1_000_000, 1_000_000)),
        ("ad", &a, &d, pool(1_000_000, 1_000_000)),
        ("db", &d, &b, pool(1_000_000, 1_000_000)),
        ("ce", &c, &e, pool(1_000_000, 1_000_000)),
    ])
}

/// Solves with a zero timeout, which skips the candidate stage entirely — so a route comes back
/// only if a reference route was built, and its swaps say which connector it went through.
async fn reference_only_route(
    market: MarketData,
    manager: &PetgraphStableDiGraphManager<DepthAndPrice>,
    algorithm_config: AlgorithmConfig,
    decomposition: DecompositionConfig,
    order: &Order,
) -> Result<RouteResult, AlgorithmError> {
    DecompositionAlgorithm::new(algorithm_config, decomposition)
        .expect("valid decomposition config")
        .find_best_route(manager.graph(), market, None, None, order)
        .await
}

#[tokio::test]
async fn test_registry_spawned_pool_derives_a_connector_token() {
    // `registry.rs` builds workers through `with_config`, which cannot set a connector token. The
    // derived default is what keeps those pools — every pool in `worker_pools.toml` — from running
    // with no reference route, no fallback and no candidate price floor.
    let (a, b, _, _) = tokens();
    let (market, manager) = two_connector_market();
    let order = order(&a, &b, 100_000, OrderSide::Sell);
    let config = AlgorithmConfig::new(1, 2, Duration::ZERO, None).expect("valid algorithm config");

    let algorithm =
        DecompositionAlgorithm::with_config(config).expect("valid decomposition config");
    let result = algorithm
        .find_best_route(manager.graph(), market, None, None, &order)
        .await
        .expect("a registry-spawned pool must still get a reference route");

    // C has an extra edge to E, so it outranks D on pool-edge degree.
    let components = route_components(&result);
    assert!(
        components.contains(&"ac") && components.contains(&"cb"),
        "expected the highest-degree connector C, got {components:?}"
    );
}

#[tokio::test]
async fn test_explicit_connector_token_beats_the_derived_default() {
    let (a, b, _, d) = tokens();
    let (market, manager) = two_connector_market();
    let order = order(&a, &b, 100_000, OrderSide::Sell);
    let config = AlgorithmConfig::new(1, 2, Duration::ZERO, None).expect("valid algorithm config");
    let decomposition = DecompositionConfig::default().with_connector_token(d.address.clone());

    let result = reference_only_route(market, &manager, config, decomposition, &order)
        .await
        .expect("the override names a usable connector");

    let components = route_components(&result);
    assert!(
        components.contains(&"ad") && components.contains(&"db"),
        "the explicit override should have won over the higher-degree C, got {components:?}"
    );
}

#[tokio::test]
async fn test_connector_allowlist_narrows_the_derived_default() {
    // With an allowlist the derived connector must come from it: C is the higher-degree hub but is
    // not allowed as an intermediate, so a reference through it would be filtered away to nothing.
    let (a, b, _, d) = tokens();
    let (market, manager) = two_connector_market();
    let order = order(&a, &b, 100_000, OrderSide::Sell);
    let allowed: FxHashSet<Address> = FxHashSet::from_iter([d.address.clone()]);
    let config = AlgorithmConfig::new(1, 2, Duration::ZERO, None)
        .expect("valid algorithm config")
        .with_connector_tokens(allowed);

    let result =
        reference_only_route(market, &manager, config, DecompositionConfig::default(), &order)
            .await
            .expect("the allowlist still offers a connector");

    let components = route_components(&result);
    assert!(
        components.contains(&"ad") && components.contains(&"db"),
        "expected the allowed connector D, got {components:?}"
    );
}

#[test]
fn test_name() {
    assert_eq!(algorithm(3, None, DecompositionConfig::default()).name(), "decomposition");
}

#[test]
fn test_with_config_carries_algorithm_config() {
    let connectors: FxHashSet<Address> = FxHashSet::from_iter([Address::from([0x01u8; 20])]);
    let config = AlgorithmConfig::new(2, 4, Duration::from_millis(750), Some(64))
        .expect("valid algorithm config")
        .with_connector_tokens(connectors.clone());

    let algorithm =
        DecompositionAlgorithm::with_config(config).expect("valid decomposition config");

    assert_eq!(algorithm.min_hops(), 2);
    assert_eq!(algorithm.max_hops(), 4);
    assert_eq!(algorithm.timeout(), Duration::from_millis(750));
    assert_eq!(algorithm.max_routes(), Some(64));
    assert_eq!(algorithm.connector_tokens(), Some(&connectors));
    assert_eq!(algorithm.config(), &DecompositionConfig::default());
}

#[test]
fn test_algorithm_max_routes_overrides_the_decomposition_cap() {
    let config =
        AlgorithmConfig::new(1, 3, Duration::from_millis(100), Some(7)).expect("valid config");
    let algorithm = DecompositionAlgorithm::new(config, DecompositionConfig::default())
        .expect("valid decomposition config");

    assert_eq!(algorithm.effective_max_routes(), 7);
}

#[test]
fn test_decomposition_cap_applies_without_an_algorithm_cap() {
    assert_eq!(
        algorithm(3, None, DecompositionConfig::default()).effective_max_routes(),
        DEFAULT_MAX_PARALLEL_ROUTES
    );
}

#[test]
fn test_zero_caps_are_rejected() {
    let config = AlgorithmConfig::default();
    for decomposition in [
        DecompositionConfig::default().with_max_parallel_routes(0),
        DecompositionConfig::default().with_max_enumerated_paths(0),
    ] {
        match DecompositionAlgorithm::new(config.clone(), decomposition) {
            Err(AlgorithmError::InvalidConfiguration { .. }) => {}
            Err(other) => panic!("expected InvalidConfiguration, got {other:?}"),
            Ok(_) => panic!("a zero cap has no answer to give"),
        }
    }
}

#[test]
fn test_computation_requirements_are_stale_only() {
    let requirements =
        algorithm(3, None, DecompositionConfig::default()).computation_requirements();

    assert!(requirements
        .fresh_requirements()
        .is_empty());
    assert!(requirements.is_required("pool_depths"));
    assert!(requirements.is_required("token_prices"));
    assert!(!requirements.is_required("spot_prices"));
}
