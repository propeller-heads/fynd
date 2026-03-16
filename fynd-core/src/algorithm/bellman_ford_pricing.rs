//! Bellman-Ford SPFA for one-to-all token pricing.
//!
//! Runs a single flat-array SPFA from a source token (e.g. WETH) with a probe amount,
//! propagating to all reachable tokens within `max_hops`. Uses forbid-revisits to prevent
//! paths through arbitrage loops that would distort prices.
//!
//! This is the pricing counterpart to the routing BF in `bellman_ford.rs`:
//! - Routing: one source, one destination, real trade amount
//! - Pricing: one source, ALL destinations, small probe amount

use std::collections::{HashMap, HashSet, VecDeque};

use num_bigint::BigUint;
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::trace;
use tycho_simulation::{
    evm::{engine_db::tycho_db::PreCachedDB, protocol::vm::state::EVMPoolState},
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::token::Token,
};

use super::AlgorithmError;
use crate::{
    feed::market_data::SharedMarketData,
    graph::petgraph::StableDiGraph,
    types::{ComponentId, Route, Swap},
};

/// Result of a one-to-all SPFA run. Contains distances and predecessors for all
/// reachable tokens from the source.
pub(crate) struct SpfaAllResult {
    distance: Vec<BigUint>,
    predecessor: Vec<Option<(NodeIndex, ComponentId)>>,
    source: NodeIndex,
    token_map: HashMap<NodeIndex, Token>,
}

impl SpfaAllResult {
    /// Returns the best forward amount reachable at `node`.
    #[allow(dead_code)]
    pub fn amount_at(&self, node: NodeIndex) -> &BigUint {
        &self.distance[node.index()]
    }

    /// Returns true if `node` was reached by the SPFA.
    pub fn is_reachable(&self, node: NodeIndex) -> bool {
        !self.distance[node.index()].is_zero()
    }

    /// Reconstructs the path from source to `dest`.
    pub fn reconstruct_path(
        &self,
        dest: NodeIndex,
    ) -> Result<Vec<(NodeIndex, NodeIndex, ComponentId)>, AlgorithmError> {
        reconstruct_path(dest, self.source, &self.predecessor)
    }

    /// Returns a reference to the token map built during subgraph extraction.
    pub fn token_map(&self) -> &HashMap<NodeIndex, Token> {
        &self.token_map
    }
}

/// Runs a flat Bellman-Ford SPFA from `source` with `amount`, propagating to all
/// reachable tokens within `max_hops`. Uses forbid-revisits to prevent paths
/// through arbitrage loops.
///
/// One forward pass prices every token reachable from the source.
pub(crate) fn solve_one_to_all(
    source: NodeIndex,
    amount: BigUint,
    max_hops: usize,
    graph: &StableDiGraph<()>,
    market: &SharedMarketData,
) -> SpfaAllResult {
    // Extract subgraph (BFS from source up to max_hops)
    let subgraph_edges = extract_subgraph(source, max_hops, graph);

    let max_idx = graph
        .node_indices()
        .map(|n| n.index())
        .max()
        .unwrap_or(0)
        + 1;

    if subgraph_edges.is_empty() {
        return SpfaAllResult {
            distance: vec![BigUint::ZERO; max_idx],
            predecessor: vec![None; max_idx],
            source,
            token_map: HashMap::new(),
        };
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

    let mut distance: Vec<BigUint> = vec![BigUint::ZERO; max_idx];
    let mut predecessor: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; max_idx];

    distance[source.index()] = amount;

    // Build adjacency list
    let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, &ComponentId)>> = HashMap::new();
    for (from, to, cid) in &subgraph_edges {
        adj.entry(*from)
            .or_default()
            .push((*to, cid));
    }

    // SPFA: seed active set with source node
    let mut active_nodes: Vec<NodeIndex> = vec![source];

    for _round in 0..max_hops {
        if active_nodes.is_empty() {
            break;
        }

        let mut next_active: HashSet<NodeIndex> = HashSet::new();

        for &u in &active_nodes {
            let u_idx = u.index();
            if distance[u_idx].is_zero() {
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

                // Forbid token revisits
                if path_contains_node(u, v, &predecessor) {
                    continue;
                }

                // Forbid pool revisits
                if path_contains_pool(u, component_id, &predecessor) {
                    continue;
                }

                let Some(token_v) = token_map.get(&v) else {
                    continue;
                };

                let Some(sim_state) = market.get_simulation_state(component_id) else {
                    continue;
                };

                let result =
                    match sim_state.get_amount_out(distance[u_idx].clone(), token_u, token_v) {
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

                if amount_out > distance[v_idx] {
                    distance[v_idx] = amount_out;
                    predecessor[v_idx] = Some((u, component_id.clone()));
                    next_active.insert(v);
                }
            }
        }

        active_nodes = next_active.into_iter().collect();
    }

    SpfaAllResult { distance, predecessor, source, token_map }
}

/// Re-simulates a path with exact amounts and state overrides for revisited pools.
pub(crate) fn resimulate_path(
    path: &[(NodeIndex, NodeIndex, ComponentId)],
    amount_in: &BigUint,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
) -> Result<(Route, BigUint), AlgorithmError> {
    let mut current_amount = amount_in.clone();
    let mut swaps = Vec::with_capacity(path.len());

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

// --- Private helpers ---

/// Extracts a subgraph via BFS from `start` up to `max_depth` hops.
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

/// Checks whether `target` node is already in the predecessor path leading to `from`.
fn path_contains_node(
    from: NodeIndex,
    target: NodeIndex,
    predecessor: &[Option<(NodeIndex, ComponentId)>],
) -> bool {
    let mut current = from;
    loop {
        if current == target {
            return true;
        }
        match &predecessor[current.index()] {
            Some((prev, _)) => current = *prev,
            None => return false,
        }
    }
}

/// Checks whether `target_pool` is already used in the predecessor path leading to `from`.
fn path_contains_pool(
    from: NodeIndex,
    target_pool: &ComponentId,
    predecessor: &[Option<(NodeIndex, ComponentId)>],
) -> bool {
    let mut current = from;
    loop {
        match &predecessor[current.index()] {
            Some((prev, cid)) => {
                if cid == target_pool {
                    return true;
                }
                current = *prev;
            }
            None => return false,
        }
    }
}

/// Reconstructs the path from dest back to source by walking the flat predecessor array.
fn reconstruct_path(
    dest: NodeIndex,
    source: NodeIndex,
    predecessor: &[Option<(NodeIndex, ComponentId)>],
) -> Result<Vec<(NodeIndex, NodeIndex, ComponentId)>, AlgorithmError> {
    let mut path = Vec::new();
    let mut current = dest;
    let mut visited = HashSet::new();

    while current != source {
        if !visited.insert(current) {
            return Err(AlgorithmError::Other(
                "cycle in predecessor chain during path reconstruction".to_string(),
            ));
        }

        let idx = current.index();
        if idx >= predecessor.len() {
            return Err(AlgorithmError::Other("predecessor index out of bounds".to_string()));
        }

        match &predecessor[idx] {
            Some((prev_node, component_id)) => {
                path.push((*prev_node, current, component_id.clone()));
                current = *prev_node;
            }
            None => {
                return Err(AlgorithmError::Other(format!(
                    "broken predecessor chain at node {idx}"
                )));
            }
        }
    }

    path.reverse();
    Ok(path)
}

#[cfg(test)]
mod tests {
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::algorithm::test_utils::{component, token, MockProtocolSim};
    use crate::graph::{GraphManager, PetgraphStableDiGraphManager};

    fn setup_market_and_graph(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (SharedMarketData, PetgraphStableDiGraphManager<()>) {
        let mut market = SharedMarketData::new();

        for (pool_id, token_in, token_out, state) in pools {
            let tokens = vec![token_in.clone(), token_out.clone()];
            let comp = component(pool_id, &tokens);
            market.upsert_components(std::iter::once(comp));
            market.update_states([(pool_id.to_string(), Box::new(state) as Box<dyn ProtocolSim>)]);
            market.upsert_tokens(tokens);
        }

        let mut graph_manager = PetgraphStableDiGraphManager::default();
        graph_manager.initialize_graph(&market.component_topology());

        (market, graph_manager)
    }

    #[test]
    fn solve_one_to_all_prices_all_reachable_tokens() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let dai = token(2, "DAI");

        let (market, gm) = setup_market_and_graph(vec![
            ("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("usdc_dai", &usdc, &dai, MockProtocolSim::new(1.0)),
        ]);

        let graph = gm.graph();
        let eth_node = graph
            .node_indices()
            .find(|&n| graph[n] == eth.address)
            .unwrap();

        let result = solve_one_to_all(eth_node, BigUint::from(100u64), 2, graph, &market);

        // ETH -> USDC: 100 * 2000 = 200_000
        let usdc_node = graph
            .node_indices()
            .find(|&n| graph[n] == usdc.address)
            .unwrap();
        assert!(result.is_reachable(usdc_node));
        assert_eq!(*result.amount_at(usdc_node), BigUint::from(200_000u64));

        // ETH -> USDC -> DAI: 200_000 * 1 = 200_000
        let dai_node = graph
            .node_indices()
            .find(|&n| graph[n] == dai.address)
            .unwrap();
        assert!(result.is_reachable(dai_node));
        assert_eq!(*result.amount_at(dai_node), BigUint::from(200_000u64));
    }

    #[test]
    fn solve_one_to_all_picks_best_path() {
        // Diamond: ETH->A->TARGET (2*3=6x) vs ETH->B->TARGET (4*1=4x)
        let eth = token(0, "ETH");
        let a = token(1, "A");
        let b = token(2, "B");
        let target = token(3, "TARGET");

        let (market, gm) = setup_market_and_graph(vec![
            ("eth_a", &eth, &a, MockProtocolSim::new(2.0)),
            ("a_target", &a, &target, MockProtocolSim::new(3.0)),
            ("eth_b", &eth, &b, MockProtocolSim::new(4.0)),
            ("b_target", &b, &target, MockProtocolSim::new(1.0)),
        ]);

        let graph = gm.graph();
        let eth_node = graph
            .node_indices()
            .find(|&n| graph[n] == eth.address)
            .unwrap();

        let result = solve_one_to_all(eth_node, BigUint::from(100u64), 2, graph, &market);

        let target_node = graph
            .node_indices()
            .find(|&n| graph[n] == target.address)
            .unwrap();

        // Best path: ETH->A->TARGET = 100*2*3 = 600
        assert_eq!(*result.amount_at(target_node), BigUint::from(600u64));
    }

    #[test]
    fn solve_one_to_all_respects_max_hops() {
        let eth = token(0, "ETH");
        let a = token(1, "A");
        let b = token(2, "B");
        let c = token(3, "C");

        let (market, gm) = setup_market_and_graph(vec![
            ("eth_a", &eth, &a, MockProtocolSim::new(2.0)),
            ("a_b", &a, &b, MockProtocolSim::new(3.0)),
            ("b_c", &b, &c, MockProtocolSim::new(4.0)),
        ]);

        let graph = gm.graph();
        let eth_node = graph
            .node_indices()
            .find(|&n| graph[n] == eth.address)
            .unwrap();

        // max_hops=2: can reach A (1 hop) and B (2 hops), but NOT C (3 hops)
        let result = solve_one_to_all(eth_node, BigUint::from(100u64), 2, graph, &market);

        let c_node = graph
            .node_indices()
            .find(|&n| graph[n] == c.address)
            .unwrap();
        assert!(!result.is_reachable(c_node), "C should not be reachable with max_hops=2");
    }

    #[test]
    fn reconstruct_and_resimulate_round_trip() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, gm) =
            setup_market_and_graph(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000.0))]);

        let graph = gm.graph();
        let eth_node = graph
            .node_indices()
            .find(|&n| graph[n] == eth.address)
            .unwrap();
        let usdc_node = graph
            .node_indices()
            .find(|&n| graph[n] == usdc.address)
            .unwrap();

        let result = solve_one_to_all(eth_node, BigUint::from(100u64), 2, graph, &market);

        // Reconstruct path
        let path = result
            .reconstruct_path(usdc_node)
            .unwrap();
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].2, "pool");

        // Re-simulate
        let (route, amount_out) =
            resimulate_path(&path, &BigUint::from(100u64), &market, result.token_map()).unwrap();
        assert_eq!(route.swaps().len(), 1);
        assert_eq!(amount_out, BigUint::from(200_000u64));
    }

    #[test]
    fn forbid_revisits_prevents_cycles() {
        // ETH -> A -> ETH would be a token revisit; should be forbidden
        let eth = token(0, "ETH");
        let a = token(1, "A");
        let b = token(2, "B");

        let (market, gm) = setup_market_and_graph(vec![
            ("eth_a", &eth, &a, MockProtocolSim::new(2.0)),
            ("a_b", &a, &b, MockProtocolSim::new(3.0)),
        ]);

        let graph = gm.graph();
        let eth_node = graph
            .node_indices()
            .find(|&n| graph[n] == eth.address)
            .unwrap();

        // With forbid-revisits, ETH should not appear as reachable
        // (it's the source, distance is set to probe amount, not a revisit result)
        let result = solve_one_to_all(eth_node, BigUint::from(100u64), 4, graph, &market);

        // A should be reachable (1 hop)
        let a_node = graph
            .node_indices()
            .find(|&n| graph[n] == a.address)
            .unwrap();
        assert!(result.is_reachable(a_node));
        assert_eq!(*result.amount_at(a_node), BigUint::from(200u64));

        // B should be reachable (2 hops)
        let b_node = graph
            .node_indices()
            .find(|&n| graph[n] == b.address)
            .unwrap();
        assert!(result.is_reachable(b_node));
        assert_eq!(*result.amount_at(b_node), BigUint::from(600u64));
    }
}
