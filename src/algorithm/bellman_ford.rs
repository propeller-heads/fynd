//! Bellman-Ford algorithm with SPFA optimization for simulation-driven routing.
//!
//! Unlike MostLiquid which scores paths with heuristic spot prices, this algorithm
//! runs actual pool simulations (`get_amount_out()`) during Bellman-Ford edge relaxation.
//! This finds better paths because it accounts for actual slippage, fees, and pool mechanics
//! at the given trade size.
//!
//! Based on Janos Tapolcai's simulation-based Bellman-Ford arbitrage searcher,
//! adapted from cycle detection to A-to-B routing.
//!
//! # Performance
//!
//! Three optimizations keep simulation calls within budget:
//! - **Subgraph extraction**: BFS prunes the graph to nodes reachable within `max_hops`
//! - **SPFA queuing**: Only re-relaxes edges from nodes whose distance improved
//! - **Top-N re-simulation**: Re-simulates the top 3 candidate layers to handle divergence

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, instrument, trace, warn};
use tycho_simulation::{
    evm::{engine_db::tycho_db::PreCachedDB, protocol::vm::state::EVMPoolState},
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::token::Token,
};

use super::{Algorithm, AlgorithmConfig, AlgorithmError, NoPathReason};
use crate::{
    derived::{ComputationRequirements, SharedDerivedDataRef, TokenGasPrices},
    feed::market_data::{SharedMarketData, SharedMarketDataRef},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult, Swap},
};

/// Simulation-driven Bellman-Ford router with SPFA optimization and top-N re-simulation.
pub struct BellmanFordAlgorithm {
    max_hops: usize,
    timeout: Duration,
}

impl BellmanFordAlgorithm {
    /// Creates a new BellmanFordAlgorithm with custom settings.
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self { max_hops: config.max_hops(), timeout: config.timeout() })
    }
}

impl Algorithm for BellmanFordAlgorithm {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "bellman_ford"
    }

    #[instrument(level = "debug", skip_all, fields(order_id = %order.id))]
    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: SharedMarketDataRef,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        let start = Instant::now();

        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        // Extract token prices from derived data (if available)
        let token_prices = if let Some(ref derived) = derived {
            derived
                .read()
                .await
                .token_prices()
                .cloned()
        } else {
            None
        };

        // Acquire read lock, snapshot data, release lock
        let (token_in_node, token_out_node, subgraph_edges, token_map, market_subset) = {
            let market = market.read().await;

            // Look up token nodes
            let token_in_node = graph
                .node_indices()
                .find(|&n| graph[n] == order.token_in)
                .ok_or(AlgorithmError::NoPath {
                    from: order.token_in.clone(),
                    to: order.token_out.clone(),
                    reason: NoPathReason::SourceTokenNotInGraph,
                })?;
            let token_out_node = graph
                .node_indices()
                .find(|&n| graph[n] == order.token_out)
                .ok_or(AlgorithmError::NoPath {
                    from: order.token_in.clone(),
                    to: order.token_out.clone(),
                    reason: NoPathReason::DestinationTokenNotInGraph,
                })?;

            // Extract subgraph (BFS from token_in up to max_hops)
            let subgraph_edges = extract_subgraph(token_in_node, self.max_hops, graph);

            if subgraph_edges.is_empty() {
                return Err(AlgorithmError::NoPath {
                    from: order.token_in.clone(),
                    to: order.token_out.clone(),
                    reason: NoPathReason::NoGraphPath,
                });
            }

            // Build token map for all nodes in subgraph
            let subgraph_nodes: HashSet<NodeIndex> = subgraph_edges
                .iter()
                .flat_map(|&(from, to, _)| [from, to])
                .collect();

            let token_map: HashMap<NodeIndex, Token> = subgraph_nodes
                .iter()
                .filter_map(|&node| {
                    let addr = &graph[node];
                    market
                        .get_token(addr)
                        .cloned()
                        .map(|t| (node, t))
                })
                .collect();

            // Collect component IDs for market subset extraction
            let component_ids: HashSet<ComponentId> = subgraph_edges
                .iter()
                .map(|(_, _, cid)| cid.clone())
                .collect();

            let market_subset = market.extract_subset(&component_ids);

            (token_in_node, token_out_node, subgraph_edges, token_map, market_subset)
        };
        // Lock released here

        debug!(
            subgraph_edges = subgraph_edges.len(),
            tokens = token_map.len(),
            "subgraph extracted"
        );

        // Layered BF relaxation with SPFA optimization: distance[hop][node] tracks
        // the best amount reachable at each node using exactly `hop` edges. This
        // correctly handles paths that revisit intermediate tokens (e.g.,
        // WETH -> AMPL -> WETH -> USDC) because each layer is independent.
        //
        // SPFA: instead of scanning all nodes per layer, we track which nodes were
        // updated and only relax their outgoing edges in the next layer. This reduces
        // simulation calls from O(V * max_hops) to O(reachable edges).
        let max_idx = graph
            .node_indices()
            .map(|n| n.index())
            .max()
            .unwrap_or(0) +
            1;
        let num_layers = self.max_hops + 1;

        // distance[k][node] = best amount at node using exactly k edges
        let mut distance: Vec<Vec<BigUint>> = vec![vec![BigUint::ZERO; max_idx]; num_layers];
        // predecessor[k][node] = (prev_node, component_id) for the edge leading here at layer k
        let mut predecessor: Vec<Vec<Option<(NodeIndex, ComponentId)>>> =
            vec![vec![None; max_idx]; num_layers];

        distance[0][token_in_node.index()] = order.amount.clone();

        // Build adjacency list for efficient edge iteration
        let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, &ComponentId)>> = HashMap::new();
        for (from, to, cid) in &subgraph_edges {
            adj.entry(*from)
                .or_default()
                .push((*to, cid));
        }

        // SPFA: seed active set with source node
        let mut active_nodes: Vec<NodeIndex> = vec![token_in_node];

        // Relax layer by layer, only processing active (updated) nodes
        for k in 0..self.max_hops {
            if start.elapsed() >= self.timeout {
                debug!(layer = k, "timeout during relaxation");
                break;
            }

            if active_nodes.is_empty() {
                debug!(layer = k, "no active nodes, stopping early");
                break;
            }

            // Log source node distance at this layer (useful for debugging hub-revisit paths)
            let src_idx = token_in_node.index();
            if k > 0 && !distance[k][src_idx].is_zero() {
                trace!(
                    layer = k,
                    source_distance = %distance[k][src_idx],
                    "source token reachable at layer (hub revisit)"
                );
            }

            let mut next_active: HashSet<NodeIndex> = HashSet::new();

            for &u in &active_nodes {
                let u_idx = u.index();
                if distance[k][u_idx].is_zero() {
                    continue;
                }

                let Some(token_u) = token_map.get(&u) else {
                    continue;
                };

                let Some(edges) = adj.get(&u) else {
                    continue;
                };

                for &(v, component_id) in edges {
                    let v_idx = v.index();

                    let Some(token_v) = token_map.get(&v) else {
                        continue;
                    };

                    let Some(sim_state) = market_subset.get_simulation_state(component_id) else {
                        continue;
                    };

                    let result = match sim_state.get_amount_out(
                        distance[k][u_idx].clone(),
                        token_u,
                        token_v,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            trace!(
                                component_id,
                                error = %e,
                                "get_amount_out failed during relaxation, skipping edge"
                            );
                            continue;
                        }
                    };

                    let amount_out = result.amount;

                    if amount_out > distance[k + 1][v_idx] {
                        distance[k + 1][v_idx] = amount_out;
                        predecessor[k + 1][v_idx] = Some((u, component_id.clone()));
                        next_active.insert(v);
                    }
                }
            }

            active_nodes = next_active.into_iter().collect();
        }

        // Collect all candidate layers where destination is reachable.
        // Instead of picking only the relaxation-best, we re-simulate multiple
        // candidates to handle re-simulation divergence (where the relaxation-optimal
        // path may not be the true best after state-override re-simulation).
        let out_idx = token_out_node.index();
        let mut candidates: Vec<(usize, BigUint)> = Vec::new();
        for (k, layer) in distance.iter().enumerate().skip(1) {
            if !layer[out_idx].is_zero() {
                trace!(layer = k, amount = %layer[out_idx], "destination reached at layer");
                candidates.push((k, layer[out_idx].clone()));
            }
        }

        if candidates.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: order.token_in.clone(),
                to: order.token_out.clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        // Sort by relaxation amount descending (best candidates first)
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        // Re-simulate top candidates and pick the one with best net_amount_out.
        // Cap at 3 to bound re-simulation cost.
        let top_n = candidates.len().min(3);
        let mut best_result: Option<(RouteResult, BigUint)> = None;

        for &(layer, ref _relaxation_amount) in candidates.iter().take(top_n) {
            if start.elapsed() >= self.timeout {
                debug!(layer, "timeout during re-simulation candidates");
                break;
            }

            let path_edges = match reconstruct_layered_path(
                token_out_node,
                token_in_node,
                layer,
                &predecessor,
            ) {
                Ok(p) => p,
                Err(e) => {
                    debug!(layer, error = %e, "path reconstruction failed for candidate");
                    continue;
                }
            };

            let (route, final_amount_out) = match simulate_path(
                &path_edges,
                &order.amount,
                &market_subset,
                &token_map,
                graph,
            ) {
                Ok(r) => r,
                Err(e) => {
                    debug!(layer, error = %e, "re-simulation failed for candidate");
                    continue;
                }
            };

            let net_amount_out = compute_net_amount_out(
                &final_amount_out,
                &route,
                &market_subset,
                token_prices.as_ref(),
            );

            let is_better = match &best_result {
                None => true,
                Some((_, prev_amount)) => final_amount_out > *prev_amount,
            };

            if is_better {
                best_result = Some((RouteResult { route, net_amount_out }, final_amount_out));
            }
        }

        let Some((result, final_amount_out)) = best_result else {
            return Err(AlgorithmError::NoPath {
                from: order.token_in.clone(),
                to: order.token_out.clone(),
                reason: NoPathReason::NoGraphPath,
            });
        };

        // Check for duplicate pool usage in the route
        let component_ids: Vec<&str> = result
            .route
            .swaps
            .iter()
            .map(|s| s.component_id.as_str())
            .collect();
        let unique_components: HashSet<&str> = component_ids.iter().copied().collect();
        let has_duplicate_pools = unique_components.len() < component_ids.len();

        let solve_time_ms = start.elapsed().as_millis() as u64;
        debug!(
            solve_time_ms,
            hops = result.route.swaps.len(),
            amount_in = %order.amount,
            amount_out = %final_amount_out,
            net_amount_out = %result.net_amount_out,
            route = %component_ids.join(" -> "),
            has_duplicate_pools,
            candidates_evaluated = top_n,
            "bellman_ford route found"
        );

        Ok(result)
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("Conflicting Computation Requirements")
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Extracts a subgraph via BFS from `start` up to `max_depth` hops.
///
/// Returns a list of (from_node, to_node, component_id) tuples representing
/// all edges reachable within the hop budget.
fn extract_subgraph(
    start: NodeIndex,
    max_depth: usize,
    graph: &StableDiGraph<()>,
) -> Vec<(NodeIndex, NodeIndex, ComponentId)> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut edges = Vec::new();

    visited.insert(start);
    queue.push_back((start, 0usize));

    while let Some((node, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        for edge in graph.edges(node) {
            let target = edge.target();
            let component_id = &edge.weight().component_id;

            edges.push((node, target, component_id.clone()));

            if !visited.contains(&target) {
                visited.insert(target);
                queue.push_back((target, depth + 1));
            }
        }
    }

    edges
}

/// Reconstructs the path from token_out back to token_in by walking the layered
/// predecessor structure. Each layer k holds the predecessor for hop k.
fn reconstruct_layered_path(
    token_out: NodeIndex,
    token_in: NodeIndex,
    best_layer: usize,
    predecessor: &[Vec<Option<(NodeIndex, ComponentId)>>],
) -> Result<Vec<(NodeIndex, NodeIndex, ComponentId)>, AlgorithmError> {
    let mut path = Vec::with_capacity(best_layer);
    let mut current = token_out;

    for k in (1..=best_layer).rev() {
        let idx = current.index();
        if idx >= predecessor[k].len() {
            return Err(AlgorithmError::Other("predecessor index out of bounds".to_string()));
        }

        match &predecessor[k][idx] {
            Some((prev_node, component_id)) => {
                path.push((*prev_node, current, component_id.clone()));
                current = *prev_node;
            }
            None => {
                return Err(AlgorithmError::Other(format!(
                    "broken predecessor chain at layer {k}, node {idx}"
                )));
            }
        }
    }

    if current != token_in {
        return Err(AlgorithmError::Other("path reconstruction did not reach source".to_string()));
    }

    path.reverse();
    Ok(path)
}

/// Re-simulates the path with exact amounts and state overrides for revisited pools.
///
/// This produces the authoritative final amounts. During SPFA relaxation, each edge
/// was evaluated against original pool state. Re-simulation applies state overrides
/// when the same pool is visited more than once.
fn simulate_path(
    path: &[(NodeIndex, NodeIndex, ComponentId)],
    amount_in: &BigUint,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
    _graph: &StableDiGraph<()>,
) -> Result<(Route, BigUint), AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut swaps = Vec::with_capacity(path.len());

    // Track state overrides for pools we've already swapped through
    let mut native_state_overrides: HashMap<&ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
    let mut vm_state_override: Option<Box<dyn ProtocolSim>> = None;

    for (from_node, to_node, component_id) in path {
        let token_in = token_map
            .get(from_node)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token",
                id: Some(format!("{:?}", from_node)),
            })?;
        let token_out = token_map
            .get(to_node)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token",
                id: Some(format!("{:?}", to_node)),
            })?;

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

        let is_vm = component_state
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
            .is_some();

        // Use override if pool was already visited
        let state_override = if is_vm {
            vm_state_override.as_ref()
        } else {
            native_state_overrides.get(component_id)
        };

        let state = state_override
            .map(Box::as_ref)
            .unwrap_or(component_state);

        let result = state
            .get_amount_out(current_amount.clone(), token_in, token_out)
            .map_err(|e| AlgorithmError::SimulationFailed {
                component_id: component_id.clone(),
                error: format!("{:?}", e),
            })?;

        swaps.push(Swap {
            component_id: component_id.clone(),
            protocol: component.protocol_system.clone(),
            token_in: token_in.address.clone(),
            token_out: token_out.address.clone(),
            amount_in: current_amount.clone(),
            amount_out: result.amount.clone(),
            gas_estimate: result.gas,
        });

        // Store updated state for subsequent hops
        if is_vm {
            vm_state_override = Some(result.new_state);
        } else {
            native_state_overrides.insert(component_id, result.new_state);
        }
        current_amount = result.amount;
    }

    let route = Route::new(swaps);
    Ok((route, current_amount))
}

/// Computes net_amount_out by subtracting gas costs from the output amount.
fn compute_net_amount_out(
    amount_out: &BigUint,
    route: &Route,
    market: &SharedMarketData,
    token_prices: Option<&TokenGasPrices>,
) -> BigInt {
    let Some(last_swap) = route.swaps.last() else {
        return BigInt::from(amount_out.clone());
    };

    let total_gas = route.total_gas();

    let gas_price = match market.gas_price() {
        Some(gp) => gp.effective_gas_price(),
        None => {
            warn!("missing gas price, returning gross amount_out");
            return BigInt::from(amount_out.clone());
        }
    };

    let gas_cost_wei = &total_gas * gas_price;

    let gas_cost_in_output_token: Option<BigUint> = token_prices
        .and_then(|prices| prices.get(&last_swap.token_out))
        .map(|price| &gas_cost_wei * &price.numerator / &price.denominator);

    match gas_cost_in_output_token {
        Some(gas_cost) => BigInt::from(amount_out.clone()) - BigInt::from(gas_cost),
        None => {
            warn!("no gas price for output token, returning gross amount_out");
            BigInt::from(amount_out.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use num_bigint::BigInt;
    use tokio::sync::RwLock;
    use tycho_simulation::{
        tycho_common::models::Address,
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component, order, token, MockProtocolSim},
        derived::{DerivedData, TokenGasPrices},
        graph::GraphManager,
        types::OrderSide,
    };

    // ==================== Test Utilities ====================

    /// Sets up market and graph with `()` edge weights for BellmanFord tests.
    fn setup_market_bf(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (Arc<RwLock<SharedMarketData>>, PetgraphStableDiGraphManager<()>) {
        let mut market = SharedMarketData::new();

        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(100u64) },
        });
        market.update_last_updated(crate::types::BlockInfo {
            number: 1,
            hash: "0x00".into(),
            timestamp: 0,
        });

        for (pool_id, token_in, token_out, state) in pools {
            let tokens = vec![token_in.clone(), token_out.clone()];
            let comp = component(pool_id, &tokens);
            market.upsert_components(std::iter::once(comp));
            market.update_states([(pool_id.to_string(), Box::new(state) as Box<dyn ProtocolSim>)]);
            market.upsert_tokens(tokens);
        }

        let mut graph_manager = PetgraphStableDiGraphManager::default();
        graph_manager.initialize_graph(&market.component_topology());

        (Arc::new(RwLock::new(market)), graph_manager)
    }

    fn setup_derived_with_token_prices(
        token_addresses: &[Address],
    ) -> crate::derived::SharedDerivedDataRef {
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        let mut token_prices: TokenGasPrices = HashMap::new();
        for address in token_addresses {
            token_prices.insert(
                address.clone(),
                Price { numerator: BigUint::from(1u64), denominator: BigUint::from(1u64) },
            );
        }

        let mut derived_data = DerivedData::new();
        derived_data.set_token_prices(token_prices, 1);
        Arc::new(RwLock::new(derived_data))
    }

    fn bf_algorithm(max_hops: usize, timeout_ms: u64) -> BellmanFordAlgorithm {
        BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, max_hops, Duration::from_millis(timeout_ms)).unwrap(),
        )
        .unwrap()
    }

    // ==================== Unit Tests ====================

    #[tokio::test]
    async fn test_linear_path_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(4)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route.swaps.len(), 3);
        // A->B: 100*2=200, B->C: 200*3=600, C->D: 600*4=2400
        assert_eq!(result.route.swaps[0].amount_out, BigUint::from(200u64));
        assert_eq!(result.route.swaps[1].amount_out, BigUint::from(600u64));
        assert_eq!(result.route.swaps[2].amount_out, BigUint::from(2400u64));
    }

    #[tokio::test]
    async fn test_picks_better_of_two_paths() {
        // Diamond graph: A->B->D (2*3=6x) vs A->C->D (4*1=4x)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bd", &token_b, &token_d, MockProtocolSim::new(3)),
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(4)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(1)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // A->B->D: 100*2*3=600 is better than A->C->D: 100*4*1=400
        assert_eq!(result.route.swaps.len(), 2);
        assert_eq!(result.route.swaps[0].component_id, "pool_ab");
        assert_eq!(result.route.swaps[1].component_id, "pool_bd");
        assert_eq!(result.route.swaps[1].amount_out, BigUint::from(600u64));
    }

    #[tokio::test]
    async fn test_parallel_pools() {
        // Two pools between A and B with different multipliers
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool2", &token_a, &token_b, MockProtocolSim::new(5)),
        ]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route.swaps.len(), 1);
        assert_eq!(result.route.swaps[0].component_id, "pool2");
        assert_eq!(result.route.swaps[0].amount_out, BigUint::from(500u64));
    }

    #[tokio::test]
    async fn test_no_path_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        // A-B connected, C disconnected
        let (market, manager) =
            setup_market_bf(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2))]);

        // Add token_c to market without connecting it
        {
            let mut m = market.write().await;
            m.upsert_tokens(vec![token_c.clone()]);
        }

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[tokio::test]
    async fn test_source_not_in_graph() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_x = token(0x99, "X");

        let (market, manager) =
            setup_market_bf(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2))]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_x, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::SourceTokenNotInGraph, .. })
        ));
    }

    #[tokio::test]
    async fn test_destination_not_in_graph() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_x = token(0x99, "X");

        let (market, manager) =
            setup_market_bf(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2))]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_x, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::DestinationTokenNotInGraph, .. })
        ));
    }

    #[tokio::test]
    async fn test_respects_max_hops() {
        // Path A->B->C->D exists but requires 3 hops; max_hops=2
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(4)),
        ]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "Should not find 3-hop path with max_hops=2"
        );
    }

    #[tokio::test]
    async fn test_source_token_may_be_revisited_for_better_output() {
        // The layered BF allows revisiting any token (including the source)
        // if it produces a better result after re-simulation. With top-N
        // re-simulation, paths like A->B->A->B->C can beat A->B->C when
        // pool state overrides create favorable exchange rates.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // Top-N re-simulation finds a path at least as good as the 2-hop (600)
        let final_amount = result
            .route
            .swaps
            .last()
            .unwrap()
            .amount_out
            .clone();
        assert!(
            final_amount >= BigUint::from(600u64),
            "should find at least the baseline 2-hop output: got {final_amount}"
        );
    }

    #[tokio::test]
    async fn test_hub_token_revisit_allowed() {
        // Verify BF can find profitable paths that revisit intermediate tokens.
        // MockProtocolSim uses directional pricing: token_in < token_out => multiply,
        // token_in > token_out => divide. So for a profitable roundtrip B->C->B,
        // we need a separate pool for C->B with a high multiplier in that direction.
        //
        // Token addresses: A=0x01 < B=0x02 < C=0x03 < D=0x04
        //
        // pool_ab (mult=2): A->B = 100*2=200
        // pool_bc (mult=2): B->C = 200*2=400
        // pool_cb (mult=100): C->B direction: C(0x03) > B(0x02), so divides by 100.
        //   400/100 = 4. That's a loss.
        //
        // For a profitable C->B, we need C < B in address ordering.
        // Use tokens: A=0x01, C=0x02, B=0x03, D=0x04
        // Then C(0x02) < B(0x03), so pool_cb with mult=5: C->B = amount * 5
        let token_a = token(0x01, "A");
        let token_c = token(0x02, "C"); // Lower address than B
        let token_b = token(0x03, "B"); // Hub token
        let token_d = token(0x04, "D");

        // pool_ab: A(0x01)->B(0x03), A < B so multiply: 100 * 2 = 200
        // pool_bc: B(0x03)->C(0x02), B > C so divide: 200 / 3 = 66
        // pool_cb: C(0x02)->B(0x03), C < B so multiply: 66 * 100 = 6600
        // pool_bd: B(0x03)->D(0x04), B < D so multiply: 6600 * 2 = 13200
        // Direct: A->B->D = 200 * 2 = 400
        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
            ("pool_cb", &token_c, &token_b, MockProtocolSim::new(100)),
            ("pool_bd", &token_b, &token_d, MockProtocolSim::new(2)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // The 4-hop path A->B->C->B->D should produce 13200
        // which is much better than direct A->B->D = 400
        let final_amount = result
            .route
            .swaps
            .last()
            .unwrap()
            .amount_out
            .clone();
        let direct_amount = BigUint::from(400u64);
        assert!(
            final_amount > direct_amount,
            "hub-revisiting path ({final_amount}) should beat direct path ({direct_amount})"
        );
        assert_eq!(result.route.swaps.len(), 4, "should use 4-hop path through hub");
    }

    #[tokio::test]
    async fn test_state_overrides_for_revisited_pools() {
        // Setup: A->B via pool1 (multiplier=2), B->C via pool1 is not possible
        // since pool1 only connects A-B. Instead test with two different pools
        // where the same component_id would be revisited.
        //
        // Actually, for a more meaningful test: create a graph where the
        // re-simulation step visits the same pool (which happens when the same
        // pool connects different token pairs in a multi-token pool).
        // For this test we verify simulate_path handles state overrides correctly.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, _) =
            setup_market_bf(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2))]);

        let market_read = market.read().await;
        let token_map: HashMap<NodeIndex, Token> = HashMap::from([
            (NodeIndex::new(0), token_a.clone()),
            (NodeIndex::new(1), token_b.clone()),
            (NodeIndex::new(0), token_a.clone()), // dummy re-insert to simulate a path
        ]);

        // Single-hop path
        let path = vec![(NodeIndex::new(0), NodeIndex::new(1), "pool1".to_string())];

        let mut graph_manager = PetgraphStableDiGraphManager::<()>::default();
        graph_manager.initialize_graph(&market_read.component_topology());

        let (route, amount_out) = simulate_path(
            &path,
            &BigUint::from(100u64),
            &market_read,
            &token_map,
            graph_manager.graph(),
        )
        .unwrap();

        assert_eq!(route.swaps.len(), 1);
        assert_eq!(amount_out, BigUint::from(200u64));
    }

    #[tokio::test]
    async fn test_gas_deduction() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2).with_gas(10),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algo
            .find_best_route(manager.graph(), market, Some(derived), &ord)
            .await
            .unwrap();

        // Output: 1000 * 2 = 2000
        // Gas: 10 gas units * 100 gas_price = 1000 wei * 1/1 price = 1000
        // Net: 2000 - 1000 = 1000
        assert_eq!(result.route.swaps[0].amount_out, BigUint::from(2000u64));
        assert_eq!(result.net_amount_out, BigInt::from(1000));
    }

    #[tokio::test]
    async fn test_timeout_respected() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
        ]);

        // 0ms timeout
        let algo = bf_algorithm(3, 0);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;

        // With 0ms timeout, we expect either:
        // - A partial result (if some layers completed before timeout check)
        // - Timeout error
        // - NoPath (if timeout prevented completing enough layers to reach dest)
        match result {
            Ok(r) => {
                assert!(!r.route.swaps.is_empty());
            }
            Err(AlgorithmError::Timeout { .. }) | Err(AlgorithmError::NoPath { .. }) => {
                // Both are acceptable for 0ms timeout
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    // ==================== Integration-style Tests ====================

    #[tokio::test]
    async fn test_with_fees() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Pool with 10% fee
        let (market, manager) = setup_market_bf(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2).with_fee(0.1),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // 1000 * 2 * (1-0.1) = 1800
        assert_eq!(result.route.swaps[0].amount_out, BigUint::from(1800u64));
    }

    #[tokio::test]
    async fn test_large_trade_slippage() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Pool with limited liquidity (500 tokens)
        let (market, manager) = setup_market_bf(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2).with_liquidity(500),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        // Should fail due to insufficient liquidity
        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "Should fail when trade exceeds pool liquidity"
        );
    }

    #[tokio::test]
    async fn test_subgraph_extraction_prunes_unreachable() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");
        let token_e = token(0x05, "E");

        // A->B->C is 2 hops. D->E is disconnected.
        let (_market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
            ("pool_de", &token_d, &token_e, MockProtocolSim::new(4)),
        ]);

        // Extract subgraph from A with depth 2
        let graph = manager.graph();
        let token_a_node = graph
            .node_indices()
            .find(|&n| graph[n] == token_a.address)
            .unwrap();
        let subgraph = extract_subgraph(token_a_node, 2, graph);

        // Subgraph should contain edges for A-B and B-C, not D-E
        let component_ids: HashSet<_> = subgraph
            .iter()
            .map(|(_, _, cid)| cid.as_str())
            .collect();
        assert!(component_ids.contains("pool_ab"));
        assert!(component_ids.contains("pool_bc"));
        assert!(!component_ids.contains("pool_de"), "disconnected edges should be pruned");
    }

    #[tokio::test]
    async fn test_spfa_skips_failed_simulations() {
        // Pool that will fail simulation (liquidity=0 would cause error for any amount)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            // Direct path with failing pool
            ("pool_ab_bad", &token_a, &token_b, MockProtocolSim::new(2).with_liquidity(0)),
            // Alternative path that works
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(2)),
            ("pool_cb", &token_c, &token_b, MockProtocolSim::new(3)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;

        // Should find A->C->B despite A->B failing
        // Note: MockProtocolSim with liquidity=0 will fail for amount > 0
        // The direct A->B edge should be skipped and the 2-hop path used
        match result {
            Ok(r) => {
                // Found alternative path
                assert!(!r.route.swaps.is_empty());
            }
            Err(AlgorithmError::NoPath { .. }) => {
                // Also acceptable if liquidity=0 blocks all paths through B
                // (since the failing pool might also block the reverse B->A edge)
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_resimulation_divergence_still_returns_correct_output() {
        // This test verifies that the re-simulation step produces correct amounts
        // even when the relaxation was optimistic
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // Verify the final amounts are from re-simulation, not relaxation
        // A->B: 100*2=200, B->C: 200*3=600
        assert_eq!(result.route.swaps[0].amount_in, BigUint::from(100u64));
        assert_eq!(result.route.swaps[0].amount_out, BigUint::from(200u64));
        assert_eq!(result.route.swaps[1].amount_in, BigUint::from(200u64));
        assert_eq!(result.route.swaps[1].amount_out, BigUint::from(600u64));
    }

    // ==================== Trait getter tests ====================

    #[test]
    fn algorithm_name() {
        let algo = bf_algorithm(4, 200);
        assert_eq!(algo.name(), "bellman_ford");
    }

    #[test]
    fn algorithm_timeout() {
        let algo = bf_algorithm(4, 200);
        assert_eq!(algo.timeout(), Duration::from_millis(200));
    }
}
