//! Petgraph's UnGraph implementation of GraphManager.
//!
//! This module provides PetgraphUnGraphManager, which implements GraphManager for
//! petgraph::graph::UnGraph, providing a reusable implementation for algorithms that use petgraph.

use std::collections::{HashMap, HashSet};

use petgraph::graph::{NodeIndex, UnGraph};
use tycho_simulation::tycho_core::models::Address;

use super::GraphManager;
use crate::{feed::events::MarketEvent, types::ComponentId};

/// Petgraph implementation of GraphManager.
///
/// This struct implements GraphManager for petgraph::graph::UnGraph.
///
/// The graph manager maintains the graph internally and updates it based on market events.
pub struct PetgraphUnGraphManager {
    // Undirected graph with token addresses as nodes and component ids as edges.
    graph: UnGraph<Address, ComponentId>,
}

impl PetgraphUnGraphManager {
    pub fn new() -> Self {
        Self { graph: UnGraph::new_undirected() }
    }
}

impl Default for PetgraphUnGraphManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphManager<UnGraph<Address, ComponentId>> for PetgraphUnGraphManager {
    // TODO: make a type alias for the component topology
    fn initialize_graph(&mut self, component_topology: &HashMap<ComponentId, Vec<Address>>) {
        // Clear existing graph
        self.graph = UnGraph::new_undirected();

        let unique_tokens: HashSet<Address> = component_topology
            .values()
            .flatten()
            .cloned()
            .collect();

        // Add all nodes (tokens) to the graph
        let mut node_map: HashMap<Address, NodeIndex> = HashMap::with_capacity(unique_tokens.len());
        for token in unique_tokens {
            let node_idx = self.graph.add_node(token.clone());
            node_map.insert(token, node_idx);
        }

        // Add edges between all tokens in each component
        for (comp_id, tokens) in component_topology {
            for (i, token_in) in tokens.iter().enumerate() {
                let from_idx = node_map[token_in];
                // Only create edges for i < j to avoid duplicate work in undirected graph
                for token_out in tokens.iter().skip(i + 1) {
                    let to_idx = node_map[token_out];
                    self.graph
                        .add_edge(from_idx, to_idx, comp_id.clone());
                }
            }
        }
    }

    fn graph(&self) -> &UnGraph<Address, ComponentId> {
        &self.graph
    }

    #[allow(unused)]
    fn handle_event(&mut self, event: &MarketEvent) {
        unimplemented!("handle_event is not implemented for PetgraphUnGraphManager");
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    /// Helper function to create a test address from a hex string.
    fn addr(s: &str) -> Address {
        Address::from_str(s).expect("Invalid address hex string")
    }

    /// Helper function to find a node index by address in a graph.
    fn find_node(graph: &UnGraph<Address, ComponentId>, addr: &Address) -> NodeIndex {
        graph
            .node_indices()
            .find(|&idx| graph[idx] == *addr)
            .expect("Token not found in graph")
    }

    #[test]
    fn test_initialize_graph_empty() {
        let mut manager = PetgraphUnGraphManager::new();
        let topology = HashMap::new();

        manager.initialize_graph(&topology);

        let graph = manager.graph();
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_initialize_graph_comprehensive() {
        let mut manager = PetgraphUnGraphManager::new();
        let mut topology = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let token_c = addr("0x6B175474E89094C44Da98b954EedeAC495271d0F"); // DAI
        let token_d = addr("0xdAC17F958D2ee523a2206206994597C13D831ec7"); // USDT

        // Pool 1: A-B-C (3-token pool, fully connected)
        topology
            .insert("pool1".to_string(), vec![token_a.clone(), token_b.clone(), token_c.clone()]);
        // Pool 2: C-D (2-token pool, overlapping with pool 1)
        topology.insert("pool2".to_string(), vec![token_c.clone(), token_d.clone()]);

        manager.initialize_graph(&topology);

        let graph = manager.graph();
        // 4 unique tokens
        assert_eq!(graph.node_count(), 4);
        // Pool 1: 3 edges (A-B, A-C, B-C)
        // Pool 2: 1 edge (C-D)
        // Total: 4 edges
        assert_eq!(graph.edge_count(), 4);

        // Verify edge labels are correct by checking specific token pairs
        let node_a = find_node(graph, &token_a);
        let node_b = find_node(graph, &token_b);
        let node_c = find_node(graph, &token_c);
        let node_d = find_node(graph, &token_d);

        // Pool 1 edges: A-B, A-C, B-C
        assert_eq!(
            graph.edge_weight(graph.find_edge(node_a, node_b).unwrap()),
            Some(&"pool1".to_string())
        );
        assert_eq!(
            graph.edge_weight(graph.find_edge(node_a, node_c).unwrap()),
            Some(&"pool1".to_string())
        );
        assert_eq!(
            graph.edge_weight(graph.find_edge(node_b, node_c).unwrap()),
            Some(&"pool1".to_string())
        );

        // Pool 2 edge: C-D
        assert_eq!(
            graph.edge_weight(graph.find_edge(node_c, node_d).unwrap()),
            Some(&"pool2".to_string())
        );
    }
}
