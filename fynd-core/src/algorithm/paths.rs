//! Finding, ranking and simulating routes that name a pool for every leg.
//!
//! A [`Path`] is a route with its pools already chosen, as opposed to the token sequences
//! [`TopologyGraph`] searches over. Enumerating one per pool combination is what an algorithm does
//! when it wants to compare whole routes rather than choose a pool per leg.

use num_bigint::{BigInt, BigUint};
use rustc_hash::FxHashMap;
use tracing::{instrument, trace};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::{token::Token, Address},
};

use super::{most_liquid::DepthAndPrice, NoPathReason};
use crate::{
    algorithm::sim_guard::GuardedProtocolSim,
    derived::types::TokenGasPrices,
    feed::market_data::{MarketData, MarketDataView, MarketState},
    graph::{GraphError, GraphQueryFilter, Path, TokenPath, TopologyGraph},
    types::{ComponentId, Route, RouteResult, Swap},
    AlgorithmError, StateLabel,
};

/// Every route between two tokens, one per combination of the pools serving its legs.
///
/// `max_paths_per_sequence` caps how many combinations each token sequence is allowed to write;
/// see [`TopologyGraph::expand_path`] for what a cap gives up.
///
/// # Errors
///
/// [`AlgorithmError::NoPath`] naming whichever of the two tokens the graph does not hold.
#[instrument(level = "debug", skip(graph, filter))]
pub(crate) fn find_paths<'a, D>(
    graph: &'a TopologyGraph<D>,
    from: &Address,
    to: &Address,
    filter: &GraphQueryFilter,
    max_paths_per_sequence: Option<usize>,
) -> Result<Vec<Path<'a, D>>, AlgorithmError> {
    let token_paths = find_token_paths(graph, from, to, filter)?;

    let mut paths = Vec::new();
    for token_path in &token_paths {
        paths.extend(graph.expand_path(token_path, max_paths_per_sequence));
    }

    Ok(paths)
}

/// Every route as a sequence of tokens, before any pool is chosen for its legs.
///
/// Wraps [`TopologyGraph::paths_between`] with the address lookups and the errors the `Algorithm`
/// trait reports.
///
/// # Errors
///
/// [`AlgorithmError::NoPath`] naming whichever of the two tokens the graph does not hold.
pub(crate) fn find_token_paths<D>(
    graph: &TopologyGraph<D>,
    from: &Address,
    to: &Address,
    filter: &GraphQueryFilter,
) -> Result<Vec<TokenPath>, AlgorithmError> {
    graph
        .paths_between(from, to, filter)
        .map_err(|error| AlgorithmError::NoPath {
            from: from.clone(),
            to: to.clone(),
            reason: match error {
                GraphError::TokenNotFound(ref missing) if missing == from => {
                    NoPathReason::SourceTokenNotInGraph
                }
                _ => NoPathReason::DestinationTokenNotInGraph,
            },
        })
}

/// Ranks a path by the rate it quotes and the liquidity behind it.
///
/// The one function here tied to an edge weight: it reads the depth and spot price
/// [`DepthAndPrice`] carries.
///
/// Formula: `score = (product of all spot_price) × min(depths)`. Spot price is the rate before
/// slippage and already includes fees; the thinnest depth stands for the bottleneck.
///
/// Higher is better. `None` when the path is empty or any edge has no weight, which means it
/// cannot be placed against the others.
pub(crate) fn try_score_path(path: &Path<DepthAndPrice>) -> Option<f64> {
    if path.is_empty() {
        trace!("cannot score empty path");
        return None;
    }

    let mut price = 1.0;
    let mut min_depth = f64::MAX;

    for edge in path.edge_iter() {
        let Some(data) = edge.data.as_ref() else {
            trace!(component_id = %edge.component_id, "edge missing weight data, path cannot be scored");
            return None;
        };

        price *= data.spot_price;
        min_depth = min_depth.min(data.depth);
    }

    Some(price * min_depth)
}

/// Swaps `amount_in` along a path, one pool at a time, and reports what comes out.
///
/// Each pool's post-swap state is carried forward, so a path crossing the same component twice
/// pays the second time what it really would.
///
/// `net_amount_out` is the output less gas priced in the output token, and can be negative when
/// the gas estimate exceeds what the route pays.
///
/// # Errors
///
/// [`AlgorithmError::DataNotFound`] for a token, component, state or gas price the market does not
/// hold, and [`AlgorithmError::Other`] when a pool refuses the swap.
#[instrument(level = "trace", skip(path, market, token_prices), fields(hop_count = path.len()))]
pub(crate) fn simulate_pool_path<D>(
    path: &Path<D>,
    market: &MarketState,
    token_prices: Option<&TokenGasPrices>,
    amount_in: BigUint,
) -> Result<RouteResult, AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut swaps = Vec::with_capacity(path.len());

    // Track state overrides for components we've already swapped through.
    let mut state_overrides: FxHashMap<&ComponentId, Box<dyn ProtocolSim>> = FxHashMap::default();
    let mut tokens: FxHashMap<Address, Token> = FxHashMap::default();

    for (address_in, edge_data, address_out) in path.iter() {
        // Get token and component data for the simulation call
        let token_in = get_token(market, address_in)?;
        let token_out = get_token(market, address_out)?;

        let component_id = &edge_data.component_id;
        let component = market
            .get_component(component_id)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "component",
                id: Some(component_id.clone()),
            })?;
        let component_state = market
            .get_simulation_state(component_id)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "simulation state",
                id: Some(component_id.clone()),
            })?;

        let state = state_overrides
            .get(component_id)
            .map(Box::as_ref)
            .unwrap_or(component_state);

        // Simulate the swap
        let result = state
            .get_amount_out_guarded(current_amount.clone(), token_in, token_out)
            .map_err(|e| AlgorithmError::Other(format!("simulation error: {:?}", e)))?;

        // Record the swap
        swaps.push(Swap::new(
            component_id.clone(),
            component.protocol_system.clone(),
            token_in.address.clone(),
            token_out.address.clone(),
            current_amount.clone(),
            result.amount.clone(),
            result.gas,
            component.clone(),
            state.clone_box(),
        ));
        tokens
            .entry(token_in.address.clone())
            .or_insert_with(|| token_in.clone());
        tokens
            .entry(token_out.address.clone())
            .or_insert_with(|| token_out.clone());

        state_overrides.insert(component_id, result.new_state);
        current_amount = result.amount;
    }

    // Calculate net amount out (output - gas cost in output token terms)
    let route = Route::new(swaps, tokens)?;
    let output_amount = route
        .swaps()
        .last()
        .map(|s| s.amount_out().clone())
        .unwrap_or_else(|| BigUint::ZERO);

    let gas_price = market
        .gas_price()
        .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
        .effective_gas_price()
        .clone();

    let net_amount_out = if let Some(last_swap) = route.swaps().last() {
        let gas_cost_wei = route.total_gas() * &gas_price;

        // Convert gas cost to output token terms using token prices. Without a price the output
        // amount stands as-is, which is what happens before derived data has been computed.
        match token_prices.and_then(|prices| prices.get(last_swap.token_out())) {
            Some(price) => {
                BigInt::from(output_amount) -
                    BigInt::from(gas_cost_wei * &price.numerator / &price.denominator)
            }
            None => BigInt::from(output_amount),
        }
    } else {
        BigInt::from(output_amount)
    };

    Ok(RouteResult::new(route, net_amount_out, gas_price))
}

/// The market's `Token` for an address.
///
/// # Errors
///
/// [`AlgorithmError::DataNotFound`] when the market does not hold it.
pub(crate) fn get_token<'a>(
    market: &'a MarketState,
    address: &Address,
) -> Result<&'a Token, AlgorithmError> {
    market
        .get_token(address)
        .ok_or_else(|| AlgorithmError::DataNotFound {
            kind: "token",
            id: Some(format!("{:?}", address)),
        })
}

/// A read view of the market under `label`, together with the gas price it was found to carry.
///
/// Returning the gas price is what makes the check below worth doing here: a caller holding the
/// view still has to get the price out of an `Option`, and would re-establish the same guarantee.
///
/// # Errors
///
/// [`AlgorithmError::Other`] when `label` names no registered overlay, and
/// [`AlgorithmError::DataNotFound`] when the market carries no gas price.
pub(crate) async fn read_market<'a>(
    market: &'a MarketData,
    label: Option<StateLabel>,
) -> Result<(MarketDataView<'a>, BigUint), AlgorithmError> {
    let view = match label.as_ref() {
        Some(l) => market
            .read_labeled(l)
            .await
            .map_err(|e| AlgorithmError::Other(e.to_string()))?,
        None => market.read().await,
    };
    let gas_price = view
        .gas_price()
        .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
        .effective_gas_price()
        .clone();
    Ok((view, gas_price))
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use rstest::rstest;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            addr,
            fixtures::{addrs, linear_graph},
            market_read, setup_market_weighted, token, MockProtocolSim,
        },
        graph::{GraphManager, TopologyGraphManager},
    };

    /// A filter with the hop budget a case needs and no connector restriction.
    fn hops(min_hops: usize, max_hops: usize) -> GraphQueryFilter {
        GraphQueryFilter { min_hops, max_hops, connector_tokens: None }
    }

    #[test]
    fn test_try_score_path_calculates_correctly() {
        let (a, b, c, _) = addrs();
        let mut m = linear_graph();

        // A->B: spot=2.0, depth=1000, fee=0.3%; B->C: spot=0.5, depth=500, fee=0.1%
        m.set_pool_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(2.0, 1000.0), false)
            .unwrap();
        m.set_pool_weight(&"bc".to_string(), &b, &c, DepthAndPrice::new(0.5, 500.0), false)
            .unwrap();

        let graph = m.graph();
        let paths = find_paths(graph, &a, &c, &hops(2, 2), None).unwrap();
        assert_eq!(paths.len(), 1);
        let path = &paths[0];

        // Spot prices multiply, the thinnest depth stands for the bottleneck.
        let expected = 2.0 * 0.5 * 500.0;
        let score = try_score_path(path).unwrap();
        assert_eq!(score, expected, "expected {expected}, got {score}");
    }

    #[test]
    fn test_try_score_path_empty_returns_none() {
        let path: Path<DepthAndPrice> = Path::new();
        assert_eq!(try_score_path(&path), None);
    }

    #[test]
    fn test_try_score_path_missing_weight_returns_none() {
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let graph = m.graph();
        let paths = find_paths(graph, &a, &b, &hops(1, 1), None).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(try_score_path(&paths[0]).is_none());
    }

    #[test]
    fn test_try_score_path_circular_route() {
        // Test scoring a circular path A -> B -> A
        let (a, b, _, _) = addrs();
        let mut m = linear_graph();

        // Set weights for both directions of the ab component
        // A->B: spot=2.0, depth=1000, fee=0.3%
        // B->A: spot=0.6, depth=800, fee=0.3%
        m.set_pool_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(2.0, 1000.0), false)
            .unwrap();
        m.set_pool_weight(&"ab".to_string(), &b, &a, DepthAndPrice::new(0.6, 800.0), false)
            .unwrap();

        let graph = m.graph();
        // Find A->B->A paths (circular, 2 hops)
        let paths = find_paths(graph, &a, &a, &hops(2, 2), None).unwrap();

        // Should find at least one path
        assert_eq!(paths.len(), 1);

        // Both directions multiply, and the thinner of the two depths bounds it.
        let score = try_score_path(&paths[0]).unwrap();
        let expected = 2.0 * 0.6 * 800.0;
        assert_eq!(score, expected, "expected {expected}, got {score}");
    }

    #[rstest]
    #[case::source_not_in_graph(false, true)]
    #[case::dest_not_in_graph(true, false)]
    fn test_find_paths_token_not_in_graph(#[case] from_exists: bool, #[case] to_exists: bool) {
        // Graph contains tokens A (0x0A) and B (0x0B) from linear_graph fixture
        let (a, b, _, _) = addrs();
        let non_existent = addr(0x99);
        let m = linear_graph();
        let g = m.graph();

        let from = if from_exists { a } else { non_existent.clone() };
        let to = if to_exists { b } else { non_existent };

        let result = find_paths(g, &from, &to, &hops(1, 3), None);

        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    // ==================== simulate_pool_path Tests ====================
    //
    // Note: These tests use MockProtocolSim which is detected as a "native" component.
    // Ideally we should also test VM component state override behavior (vm_state_override),
    // which shares state across all VM components. This would require a mock that
    // downcasts to EVMPoolState<PreCachedDB>, or integration tests with real VM components.

    #[test]
    fn test_simulate_path_single_hop() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        let filter = hops(1, 1);
        let paths =
            find_paths(manager.graph(), &token_a.address, &token_b.address, &filter, None).unwrap();
        let path = paths.into_iter().next().unwrap();

        let result = simulate_pool_path(
            &path,
            market_read(&market).base_market_state(),
            None,
            BigUint::from(100u64),
        )
        .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(*result.route().swaps()[0].amount_in(), BigUint::from(100u64));
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(200u64)); // 100 * 2
        assert_eq!(result.route().swaps()[0].component_id(), "component1");
    }

    #[test]
    fn test_simulate_path_multi_hop_chains_amounts() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_weighted(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component2", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let filter = hops(2, 2);
        let paths =
            find_paths(manager.graph(), &token_a.address, &token_c.address, &filter, None).unwrap();
        let path = paths.into_iter().next().unwrap();

        let result = simulate_pool_path(
            &path,
            market_read(&market).base_market_state(),
            None,
            BigUint::from(10u64),
        )
        .unwrap();

        assert_eq!(result.route().swaps().len(), 2);
        // First hop: 10 * 2 = 20
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(20u64));
        // Second hop: 20 * 3 = 60
        assert_eq!(*result.route().swaps()[1].amount_in(), BigUint::from(20u64));
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(60u64));
    }

    #[test]
    fn test_simulate_path_same_component_twice_uses_updated_state() {
        // Route: A -> B -> A through the same component
        // First swap uses multiplier=2, second should use multiplier=3 (updated state)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        // A->B->A path requires min_hops=2, max_hops=2
        // Since the graph is bidirectional, we should get A->B->A path
        let filter = hops(2, 2);
        let paths =
            find_paths(manager.graph(), &token_a.address, &token_a.address, &filter, None).unwrap();

        // Should only contain the A->B->A path
        assert_eq!(paths.len(), 1);
        let path = paths[0].clone();

        let result = simulate_pool_path(
            &path,
            market_read(&market).base_market_state(),
            None,
            BigUint::from(10u64),
        )
        .unwrap();

        assert_eq!(result.route().swaps().len(), 2);
        // First: 10 * 2 = 20
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(20u64));
        // Second: 20 / 3 = 6 (state updated, multiplier incremented)
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(6u64));
    }

    #[test]
    fn test_simulate_path_missing_token_returns_data_not_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, _) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);
        let market = market_read(&market);

        // Add token C to graph but not to market (A->B->C)
        let mut topology = market.component_topology();
        topology.insert(
            "component2".to_string(),
            vec![token_b.address.clone(), token_c.address.clone()],
        );
        let mut manager = TopologyGraphManager::<DepthAndPrice>::default();
        manager.initialize_graph(&topology);

        let graph = manager.graph();
        let filter = hops(2, 2);
        let paths = find_paths(graph, &token_a.address, &token_c.address, &filter, None).unwrap();
        let path = paths.into_iter().next().unwrap();

        let result =
            simulate_pool_path(&path, market.base_market_state(), None, BigUint::from(100u64));
        assert!(matches!(result, Err(AlgorithmError::DataNotFound { kind: "token", .. })));
    }

    #[test]
    fn test_simulate_path_missing_component_returns_data_not_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        // Remove the component but keep tokens and graph
        let mut market_write = market.try_write().unwrap();
        market_write.remove_components([&"component1".to_string()]);
        drop(market_write);

        let graph = manager.graph();
        let filter = hops(1, 1);
        let paths = find_paths(graph, &token_a.address, &token_b.address, &filter, None).unwrap();
        let path = paths.into_iter().next().unwrap();

        let result = simulate_pool_path(
            &path,
            market_read(&market).base_market_state(),
            None,
            BigUint::from(100u64),
        );
        assert!(matches!(result, Err(AlgorithmError::DataNotFound { kind: "component", .. })));
    }
}
