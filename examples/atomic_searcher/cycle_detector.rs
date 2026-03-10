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
use tracing::debug;
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

    // Check all edges pointing back to source to find profitable cycles.
    // A cycle exists if: source -> ... -> node -[edge]-> source, and the
    // final amount exceeds the seed amount.
    let source_token = match token_map.get(&source_node) {
        Some(t) => t,
        None => return Vec::new(),
    };

    // Scan all layers for non-zero distances and check closing edges.
    let candidates = find_closing_edges(
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
    dedup_candidates(candidates)
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
                            _layer: k + 1,
                        });
                    }
                }
            }
        }
    }

    candidates
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
///
/// Nodes in `blacklisted_nodes` are excluded: edges into them are dropped and
/// they are never enqueued, so the BFS never traverses through them.
pub fn extract_subgraph(
    start: NodeIndex,
    max_depth: usize,
    graph: &StableDiGraph<()>,
    blacklisted_nodes: &HashSet<NodeIndex>,
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

            if blacklisted_nodes.contains(&target) {
                continue;
            }

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::any::Any;
    use std::collections::HashMap;

    use chrono::NaiveDateTime;
    use num_bigint::BigUint;
    use tycho_simulation::tycho_core::{
        dto::ProtocolStateDelta,
        models::{protocol::ProtocolComponent, token::Token, Address, Chain},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{
                Balances, GetAmountOutResult, PoolSwap, ProtocolSim,
                QueryPoolSwapParams,
            },
        },
        Bytes,
    };

    use fynd::{
        feed::market_data::SharedMarketData,
        graph::{petgraph::PetgraphStableDiGraphManager, GraphManager},
    };

    // ==================== Local MockProtocolSim ====================
    //
    // Mirrors fynd::algorithm::test_utils::MockProtocolSim (which is
    // pub(crate) and inaccessible from examples).

    #[derive(Debug, Clone)]
    struct MockSim {
        spot_price: u32,
        gas: u64,
    }

    impl MockSim {
        fn new(spot_price: u32) -> Self {
            Self { spot_price, gas: 50_000 }
        }
    }

    impl ProtocolSim for MockSim {
        fn fee(&self) -> f64 {
            0.0
        }

        fn spot_price(
            &self,
            base: &Token,
            quote: &Token,
        ) -> Result<f64, SimulationError> {
            if base.address < quote.address {
                Ok(1.0 / self.spot_price as f64)
            } else {
                Ok(self.spot_price as f64)
            }
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            let amount_out = if token_in.address < token_out.address {
                &amount_in * self.spot_price
            } else {
                &amount_in / self.spot_price
            };
            let new_state = Box::new(MockSim {
                spot_price: self.spot_price + 1,
                gas: self.gas,
            });
            Ok(GetAmountOutResult::new(
                amount_out,
                BigUint::from(self.gas),
                new_state,
            ))
        }

        fn query_pool_swap(
            &self,
            _params: &QueryPoolSwapParams,
        ) -> Result<PoolSwap, SimulationError> {
            unimplemented!()
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            unimplemented!()
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError<String>> {
            unimplemented!()
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .map(|o| o.spot_price == self.spot_price)
                .unwrap_or(false)
        }
    }

    // ==================== Test Helpers ====================

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    fn token(addr_b: u8, symbol: &str) -> Token {
        Token {
            address: addr(addr_b),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn component(id: &str, tokens: &[Token]) -> ProtocolComponent {
        ProtocolComponent::new(
            id,
            "uniswap_v2",
            "swap",
            Chain::Ethereum,
            tokens.iter().map(|t| t.address.clone()).collect(),
            vec![],
            HashMap::new(),
            Default::default(),
            Default::default(),
            NaiveDateTime::default(),
        )
    }

    /// Builds a market, graph, and token map for a triangle:
    ///   A(0x01) <-> B(0x02) <-> C(0x03) <-> A(0x01)
    ///
    /// Spot prices are configurable per pool.
    fn setup_triangle(
        sp_ab: u32,
        sp_bc: u32,
        sp_ca: u32,
    ) -> (
        SharedMarketData,
        PetgraphStableDiGraphManager<()>,
        HashMap<NodeIndex, Token>,
        Token,
        Token,
        Token,
    ) {
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        let tok_c = token(0x03, "C");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);
        let comp_bc = component("pool_bc", &[tok_b.clone(), tok_c.clone()]);
        let comp_ca = component("pool_ca", &[tok_c.clone(), tok_a.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components(
            [comp_ab, comp_bc, comp_ca].into_iter(),
        );
        market.update_states([
            ("pool_ab".to_string(), Box::new(MockSim::new(sp_ab)) as Box<dyn ProtocolSim>),
            ("pool_bc".to_string(), Box::new(MockSim::new(sp_bc)) as Box<dyn ProtocolSim>),
            ("pool_ca".to_string(), Box::new(MockSim::new(sp_ca)) as Box<dyn ProtocolSim>),
        ]);
        market.upsert_tokens([tok_a.clone(), tok_b.clone(), tok_c.clone()]);

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let token_map: HashMap<NodeIndex, Token> = graph
            .node_indices()
            .filter_map(|n| {
                let a = &graph[n];
                market.get_token(a).cloned().map(|t| (n, t))
            })
            .collect();

        (market, gm, token_map, tok_a, tok_b, tok_c)
    }

    // ==================== Tests ====================

    #[test]
    fn test_simple_cycle_found() {
        // Triangle A->B->C->A with spot prices that make the cycle
        // profitable:
        //   A->B: amount * 2   (0x01 < 0x02)
        //   B->C: amount * 3   (0x02 < 0x03)
        //   C->A: amount / 1   (0x03 > 0x01)
        // Product = 2 * 3 / 1 = 6 > 1, so the cycle is profitable.
        let (market, gm, token_map, tok_a, _, _) =
            setup_triangle(2, 3, 1);

        let graph = gm.graph();
        let source_node = graph
            .node_indices()
            .find(|&n| graph[n] == tok_a.address)
            .unwrap();

        let seed = BigUint::from(1000u64);
        let subgraph_edges = extract_subgraph(source_node, 4, graph, &HashSet::new());

        let candidates = find_cycles(
            source_node,
            &seed,
            4,
            graph,
            &market,
            &token_map,
            &subgraph_edges,
        );

        assert!(
            !candidates.is_empty(),
            "should find at least one profitable cycle"
        );

        // The cycle should produce seed * 2 * 3 / 1 = 6000 > 1000
        let best = &candidates[0];
        assert!(
            best.relaxation_amount_out > seed,
            "cycle output {} should exceed seed {}",
            best.relaxation_amount_out,
            seed,
        );
        assert_eq!(best.edges.len(), 3, "triangle has 3 edges");
    }

    #[test]
    fn test_no_cycle_when_unprofitable() {
        // Triangle where ALL directions are unprofitable.
        // With sp=1 on every pool:
        //   Forward  A->B *1, B->C *1, C->A /1 => product = 1 (not > 1)
        //   Reverse  A->C *1, C->B /1, B->A /1 => product = 1 (not > 1)
        // Note: the graph is bidirectional, so both directions are
        // checked. Using sp=1 everywhere ensures no direction wins.
        let (market, gm, token_map, tok_a, _, _) =
            setup_triangle(1, 1, 1);

        let graph = gm.graph();
        let source_node = graph
            .node_indices()
            .find(|&n| graph[n] == tok_a.address)
            .unwrap();

        let seed = BigUint::from(1000u64);
        let subgraph_edges = extract_subgraph(source_node, 4, graph, &HashSet::new());

        let candidates = find_cycles(
            source_node,
            &seed,
            4,
            graph,
            &market,
            &token_map,
            &subgraph_edges,
        );

        assert!(
            candidates.is_empty(),
            "no profitable cycle should be found, but got {} candidates",
            candidates.len(),
        );
    }

    #[test]
    fn test_empty_graph_no_cycles() {
        // No edges in subgraph => no cycles.
        let tok_a = token(0x01, "A");
        let mut market = SharedMarketData::new();
        market.upsert_tokens([tok_a.clone()]);

        // Graph with a single node and no edges.
        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        let mut topo = HashMap::new();
        topo.insert("empty".to_string(), vec![tok_a.address.clone()]);
        gm.initialize_graph(&topo);

        let graph = gm.graph();
        let source_node = graph
            .node_indices()
            .find(|&n| graph[n] == tok_a.address)
            .unwrap();

        let token_map: HashMap<NodeIndex, Token> =
            [(source_node, tok_a)].into_iter().collect();

        let seed = BigUint::from(1000u64);
        let subgraph_edges: Vec<(NodeIndex, NodeIndex, String)> = vec![];

        let candidates = find_cycles(
            source_node,
            &seed,
            4,
            graph,
            &market,
            &token_map,
            &subgraph_edges,
        );

        assert!(candidates.is_empty());
    }

    #[test]
    fn test_multi_hop_cycle() {
        // 4-hop cycle: A->B->C->D->A
        //   A->B: amount * 2   (0x01 < 0x02)
        //   B->C: amount * 2   (0x02 < 0x03)
        //   C->D: amount * 2   (0x03 < 0x04)
        //   D->A: amount / 1   (0x04 > 0x01)
        // Product = 2 * 2 * 2 / 1 = 8 > 1
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        let tok_c = token(0x03, "C");
        let tok_d = token(0x04, "D");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);
        let comp_bc = component("pool_bc", &[tok_b.clone(), tok_c.clone()]);
        let comp_cd = component("pool_cd", &[tok_c.clone(), tok_d.clone()]);
        let comp_da = component("pool_da", &[tok_d.clone(), tok_a.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components(
            [comp_ab, comp_bc, comp_cd, comp_da].into_iter(),
        );
        market.update_states([
            ("pool_ab".to_string(), Box::new(MockSim::new(2)) as Box<dyn ProtocolSim>),
            ("pool_bc".to_string(), Box::new(MockSim::new(2)) as Box<dyn ProtocolSim>),
            ("pool_cd".to_string(), Box::new(MockSim::new(2)) as Box<dyn ProtocolSim>),
            ("pool_da".to_string(), Box::new(MockSim::new(1)) as Box<dyn ProtocolSim>),
        ]);
        market.upsert_tokens([
            tok_a.clone(),
            tok_b.clone(),
            tok_c.clone(),
            tok_d.clone(),
        ]);

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let source_node = graph
            .node_indices()
            .find(|&n| graph[n] == tok_a.address)
            .unwrap();

        let token_map: HashMap<NodeIndex, Token> = graph
            .node_indices()
            .filter_map(|n| {
                market.get_token(&graph[n]).cloned().map(|t| (n, t))
            })
            .collect();

        let seed = BigUint::from(100u64);
        let subgraph_edges = extract_subgraph(source_node, 5, graph, &HashSet::new());

        let candidates = find_cycles(
            source_node,
            &seed,
            5,
            graph,
            &market,
            &token_map,
            &subgraph_edges,
        );

        assert!(
            !candidates.is_empty(),
            "should find the 4-hop profitable cycle"
        );

        // Check that we found a 4-edge cycle
        let has_4hop = candidates.iter().any(|c| c.edges.len() == 4);
        assert!(
            has_4hop,
            "expected a 4-edge cycle among candidates: {:?}",
            candidates
                .iter()
                .map(|c| c.edges.len())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_extract_subgraph_respects_depth() {
        // Chain: A -> B -> C -> D (each pool has both directions).
        // BFS from A with depth=1 should only reach B.
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        let tok_c = token(0x03, "C");
        let tok_d = token(0x04, "D");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);
        let comp_bc = component("pool_bc", &[tok_b.clone(), tok_c.clone()]);
        let comp_cd = component("pool_cd", &[tok_c.clone(), tok_d.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components([comp_ab, comp_bc, comp_cd].into_iter());

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let source_node = graph
            .node_indices()
            .find(|&n| graph[n] == tok_a.address)
            .unwrap();

        // Depth 1: should reach only A and B
        let edges_d1 = extract_subgraph(source_node, 1, graph, &HashSet::new());
        let nodes_d1: HashSet<NodeIndex> = edges_d1
            .iter()
            .flat_map(|(f, t, _)| [*f, *t])
            .collect();
        // A has edges to B (and B to A from the bidirectional pool).
        // D should NOT be reachable at depth 1.
        let d_node = graph
            .node_indices()
            .find(|&n| graph[n] == tok_d.address)
            .unwrap();
        assert!(
            !nodes_d1.contains(&d_node),
            "D should not be in depth-1 subgraph"
        );

        // Depth 3: should reach all nodes including D
        let edges_d3 = extract_subgraph(source_node, 3, graph, &HashSet::new());
        let nodes_d3: HashSet<NodeIndex> = edges_d3
            .iter()
            .flat_map(|(f, t, _)| [*f, *t])
            .collect();
        assert!(
            nodes_d3.contains(&d_node),
            "D should be in depth-3 subgraph"
        );
    }

    #[test]
    fn test_dedup_removes_duplicate_cycles() {
        let addr_a = addr(0x01);
        let addr_b = addr(0x02);

        let c1 = CycleCandidate {
            edges: vec![
                (addr_a.clone(), addr_b.clone(), "pool_1".into()),
                (addr_b.clone(), addr_a.clone(), "pool_2".into()),
            ],
            relaxation_amount_out: BigUint::from(200u64),
            _layer: 2,
        };
        // Same component path, different relaxation amount
        let c2 = CycleCandidate {
            edges: vec![
                (addr_a.clone(), addr_b.clone(), "pool_1".into()),
                (addr_b.clone(), addr_a.clone(), "pool_2".into()),
            ],
            relaxation_amount_out: BigUint::from(100u64),
            _layer: 3,
        };
        // Different path
        let c3 = CycleCandidate {
            edges: vec![
                (addr_a.clone(), addr_b.clone(), "pool_3".into()),
                (addr_b.clone(), addr_a.clone(), "pool_4".into()),
            ],
            relaxation_amount_out: BigUint::from(50u64),
            _layer: 2,
        };

        let result = dedup_candidates(vec![c1, c2, c3]);
        assert_eq!(result.len(), 2, "duplicate should be removed");
        // First should be the one with highest relaxation_amount_out
        assert_eq!(result[0].relaxation_amount_out, BigUint::from(200u64));
        assert_eq!(result[1].relaxation_amount_out, BigUint::from(50u64));
    }
}
