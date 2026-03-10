//! Cycle detection via layered Bellman-Ford relaxation.
//!
//! Adapted from Fynd's BellmanFordAlgorithm (which finds A-to-B routes)
//! to find profitable cycles: Token_A -> ... -> Token_A.
//!
//! Based on Janos Tapolcai's simulation-based Bellman-Ford arbitrage searcher.
//! See: <https://github.com/jtapolcai/tycho-searcher>

use std::collections::{HashMap, HashSet, VecDeque};

use num_bigint::BigUint;
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, trace};
use tycho_simulation::tycho_common::models::Address;
use tycho_simulation::tycho_core::models::token::Token;

use fynd::feed::market_data::SharedMarketData;
use fynd::graph::petgraph::StableDiGraph;

use crate::types::CycleCandidate;

/// Finds arbitrage cycles starting and ending at `source_node`.
///
/// Runs a layered Bellman-Ford relaxation (same as the solver) but instead of
/// collecting the best path to a destination, collects paths that return to the
/// source with more tokens than they started with.
pub fn find_cycles(
    source_node: NodeIndex,
    seed_amount: &BigUint,
    max_hops: usize,
    graph: &StableDiGraph<()>,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
    subgraph_edges: &[(NodeIndex, NodeIndex, String)],
) -> Vec<CycleCandidate> {
    let max_idx = graph
        .node_indices()
        .map(|n| n.index())
        .max()
        .unwrap_or(0)
        + 1;
    let num_layers = max_hops + 1;

    // distance[k][node] = best amount at node using exactly k edges from source
    let mut distance: Vec<Vec<BigUint>> = vec![vec![BigUint::ZERO; max_idx]; num_layers];
    // predecessor[k][node] = (prev_node, component_id, prev_layer)
    let mut predecessor: Vec<Vec<Option<(NodeIndex, String)>>> =
        vec![vec![None; max_idx]; num_layers];

    let src_idx = source_node.index();
    distance[0][src_idx] = seed_amount.clone();

    // Build adjacency list
    let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, &String)>> = HashMap::new();
    for (from, to, cid) in subgraph_edges {
        adj.entry(*from).or_default().push((*to, cid));
    }

    // SPFA: seed active set with source node
    let mut active_nodes: Vec<NodeIndex> = vec![source_node];

    // Relax layer by layer
    for k in 0..max_hops {
        if active_nodes.is_empty() {
            debug!(layer = k, "no active nodes, stopping early");
            break;
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
                // Skip edges back to source during relaxation; we check them after
                if v == source_node {
                    continue;
                }

                let v_idx = v.index();

                let Some(token_v) = token_map.get(&v) else {
                    continue;
                };

                let Some(sim_state) = market.get_simulation_state(component_id) else {
                    continue;
                };

                let result = match sim_state.get_amount_out(
                    distance[k][u_idx].clone(),
                    token_u,
                    token_v,
                ) {
                    Ok(r) => r,
                    Err(_) => continue,
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

    // Now check all edges pointing back to source to find profitable cycles.
    // A cycle exists if: source -> ... -> node -[edge]-> source, and the
    // final amount exceeds the seed amount.
    let mut candidates: Vec<CycleCandidate> = Vec::new();
    let source_token = match token_map.get(&source_node) {
        Some(t) => t,
        None => return candidates,
    };

    for k in 1..num_layers {
        for &u in active_nodes_at_layer(k, &distance, max_idx).iter() {
            let u_idx = u.index();
            if distance[k][u_idx].is_zero() {
                continue;
            }

            let Some(token_u) = token_map.get(&u) else {
                continue;
            };

            // Check edges from u back to source
            let Some(edges) = adj.get(&u) else {
                continue;
            };

            for &(v, component_id) in edges {
                if v != source_node {
                    continue;
                }

                let Some(sim_state) = market.get_simulation_state(component_id) else {
                    continue;
                };

                let result = match sim_state.get_amount_out(
                    distance[k][u_idx].clone(),
                    token_u,
                    source_token,
                ) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                // Check if we get back more than we started with
                if result.amount > *seed_amount {
                    let path = match reconstruct_cycle(
                        u,
                        source_node,
                        k,
                        &predecessor,
                        component_id,
                        graph,
                    ) {
                        Some(p) => p,
                        None => continue,
                    };

                    trace!(
                        layer = k + 1,
                        amount_out = %result.amount,
                        seed = %seed_amount,
                        "cycle candidate found"
                    );

                    candidates.push(CycleCandidate {
                        edges: path,
                        relaxation_amount_out: result.amount,
                        layer: k + 1,
                    });
                }
            }
        }
    }

    // Also check nodes from all layers (not just active ones at last layer)
    // by scanning all layers for non-zero distances
    let mut all_candidates = find_closing_edges(
        &distance,
        &predecessor,
        &adj,
        source_node,
        source_token,
        seed_amount,
        market,
        token_map,
        graph,
        max_hops,
        max_idx,
    );
    // Merge, dedup by path
    all_candidates.extend(candidates);
    dedup_candidates(all_candidates)
}

/// Finds all edges that close a cycle back to source, scanning all layers.
fn find_closing_edges(
    distance: &[Vec<BigUint>],
    predecessor: &[Vec<Option<(NodeIndex, String)>>],
    adj: &HashMap<NodeIndex, Vec<(NodeIndex, &String)>>,
    source_node: NodeIndex,
    source_token: &Token,
    seed_amount: &BigUint,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
    graph: &StableDiGraph<()>,
    max_hops: usize,
    max_idx: usize,
) -> Vec<CycleCandidate> {
    let mut candidates = Vec::new();

    for k in 1..=max_hops {
        for node_idx in 0..max_idx {
            if distance[k][node_idx].is_zero() {
                continue;
            }

            let u = NodeIndex::new(node_idx);
            if u == source_node {
                continue;
            }

            let Some(token_u) = token_map.get(&u) else {
                continue;
            };

            let Some(edges) = adj.get(&u) else {
                continue;
            };

            for &(v, component_id) in edges {
                if v != source_node {
                    continue;
                }

                let Some(sim_state) = market.get_simulation_state(component_id) else {
                    continue;
                };

                let result = match sim_state.get_amount_out(
                    distance[k][node_idx].clone(),
                    token_u,
                    source_token,
                ) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                if result.amount > *seed_amount {
                    if let Some(path) = reconstruct_cycle(
                        u,
                        source_node,
                        k,
                        predecessor,
                        component_id,
                        graph,
                    ) {
                        candidates.push(CycleCandidate {
                            edges: path,
                            relaxation_amount_out: result.amount,
                            layer: k + 1,
                        });
                    }
                }
            }
        }
    }

    candidates
}

/// Helper to collect nodes with non-zero distance at a given layer.
fn active_nodes_at_layer(layer: usize, distance: &[Vec<BigUint>], max_idx: usize) -> Vec<NodeIndex> {
    (0..max_idx)
        .filter(|&idx| !distance[layer][idx].is_zero())
        .map(NodeIndex::new)
        .collect()
}

/// Reconstructs a cycle path from predecessor data.
///
/// Walks backward from `last_node` at `layer` to `source_node`, then appends
/// the closing edge (last_node -> source via closing_component).
fn reconstruct_cycle(
    last_node: NodeIndex,
    source_node: NodeIndex,
    layer: usize,
    predecessor: &[Vec<Option<(NodeIndex, String)>>],
    closing_component: &str,
    graph: &StableDiGraph<()>,
) -> Option<Vec<(Address, Address, String)>> {
    let mut path_nodes: Vec<(NodeIndex, String)> = Vec::new();

    // Walk backward through predecessors
    let mut current = last_node;
    for k in (1..=layer).rev() {
        let idx = current.index();
        if idx >= predecessor[k].len() {
            return None;
        }
        match &predecessor[k][idx] {
            Some((prev, cid)) => {
                path_nodes.push((current, cid.clone()));
                current = *prev;
            }
            None => return None,
        }
    }

    if current != source_node {
        return None;
    }

    path_nodes.reverse();

    // Build edge list: source -> node1 -> ... -> last_node -> source
    let mut edges = Vec::with_capacity(path_nodes.len() + 1);
    let mut from = source_node;
    for (to, cid) in &path_nodes {
        edges.push((graph[from].clone(), graph[*to].clone(), cid.clone()));
        from = *to;
    }
    // Closing edge
    edges.push((graph[last_node].clone(), graph[source_node].clone(), closing_component.to_string()));

    Some(edges)
}

/// Extracts a subgraph via BFS from `start` up to `max_depth` hops.
pub fn extract_subgraph(
    start: NodeIndex,
    max_depth: usize,
    graph: &StableDiGraph<()>,
) -> Vec<(NodeIndex, NodeIndex, String)> {
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

/// Deduplicates cycle candidates by their edge path.
fn dedup_candidates(mut candidates: Vec<CycleCandidate>) -> Vec<CycleCandidate> {
    let mut seen: HashSet<Vec<String>> = HashSet::new();
    candidates.retain(|c| {
        let key: Vec<String> = c.edges.iter().map(|(_, _, cid)| cid.clone()).collect();
        seen.insert(key)
    });
    // Sort by relaxation amount descending
    candidates.sort_by(|a, b| b.relaxation_amount_out.cmp(&a.relaxation_amount_out));
    candidates
}
