//! Petgraph's UnGraph implementation of GraphManager.
//!
//! This module provides PetgraphUnGraphManager, which implements GraphManager for
//! petgraph::graph::UnGraph, providing a reusable implementation for algorithms that use petgraph.

use std::collections::{HashMap, HashSet};

use petgraph::graph::{DiGraph, EdgeIndex, NodeIndex};
use tycho_simulation::tycho_core::models::Address;

use super::GraphManager;
use crate::{
    feed::events::{MarketEvent, MarketEventHandler},
    types::ComponentId,
};

#[derive(Debug, Clone, PartialEq)]
pub enum EdgeWeight {
    Depth(f64),
    SpotPrice(f64),
}

impl Default for EdgeWeight {
    /// Returns a default weight of 0.0 (Depth variant).
    fn default() -> Self {
        EdgeWeight::Depth(0.0)
    }
}

impl EdgeWeight {
    /// Extracts the numeric weight value for use in algorithms.
    pub fn as_f64(&self) -> f64 {
        match self {
            EdgeWeight::Depth(v) | EdgeWeight::SpotPrice(v) => *v,
        }
    }
}

/// Edge data containing both component ID and weight.
#[derive(Debug, Clone)]
pub struct EdgeData {
    /// The component ID that enables this swap.
    pub component_id: ComponentId,
    /// The weight of this edge
    pub weight: EdgeWeight,
}

impl EdgeData {
    /// Creates a new EdgeData with the given component ID and default weight.
    pub fn new(component_id: ComponentId) -> Self {
        Self { component_id, weight: EdgeWeight::default() }
    }

    /// Extracts the numeric weight value for use in algorithms.
    /// This is a convenience method that calls `weight.as_f64()`.
    pub fn weight_as_f64(&self) -> f64 {
        self.weight.as_f64()
    }
}

/// Petgraph implementation of GraphManager.
///
/// This struct implements GraphManager for petgraph::graph::UnGraph.
///
/// The graph manager maintains the graph internally and updates it based on market events.
pub struct PetgraphGraphManager {
    // Directed graph with token addresses as nodes and edge data (component id + weight) as edges.
    graph: DiGraph<Address, EdgeData>,
    // Map from ComponentId to edge indices for fast removal and weight updates
    edge_map: HashMap<ComponentId, Vec<EdgeIndex>>,
}

impl PetgraphGraphManager {
    pub fn new() -> Self {
        Self { graph: DiGraph::new(), edge_map: HashMap::new() }
    }

    /// Helper function to find a node index by address
    fn find_node(&self, addr: &Address) -> Result<NodeIndex, GraphError> {
        Ok(self
            .graph
            .node_indices()
            .find(|&idx| self.graph[idx] == *addr)
            .expect("Token not found in graph"))
    }

    }
}

impl Default for PetgraphGraphManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphManager<DiGraph<Address, EdgeData>> for PetgraphGraphManager {
    // TODO: make a type alias for the component topology
    fn initialize_graph(&mut self, component_topology: &HashMap<ComponentId, Vec<Address>>) {
        // Clear existing graph and component map
        self.graph = DiGraph::new();
        self.edge_map.clear();

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

        // Add bidirectional edges between all tokens in each component
        // For each component, create edges in both directions for each token pair
        for (comp_id, tokens) in component_topology {
            for (i, token_a) in tokens.iter().enumerate() {
                let from_idx = node_map[token_a];
                // Create edges for all pairs (i < j) to avoid duplicates
                for token_b in tokens.iter().skip(i + 1) {
                    let to_idx = node_map[token_b];
                    // Create edge A -> B
                    let edge_idx_ab =
                        self.graph
                            .add_edge(from_idx, to_idx, EdgeData::new(comp_id.clone()));
                    self.edge_map
                        .entry(comp_id.clone())
                        .or_default()
                        .push(edge_idx_ab);
                    // Create edge B -> A
                    let edge_idx_ba =
                        self.graph
                            .add_edge(to_idx, from_idx, EdgeData::new(comp_id.clone()));
                    self.edge_map
                        .entry(comp_id.clone())
                        .or_default()
                        .push(edge_idx_ba);
                }
            }
        }
    }

    fn graph(&self) -> &DiGraph<Address, EdgeData> {
        &self.graph
    }
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

    #[test]
    fn test_initialize_graph_empty() {
        let mut manager = PetgraphGraphManager::new();
        let topology = HashMap::new();

        manager.initialize_graph(&topology);

        let graph = manager.graph();
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_initialize_graph_comprehensive() {
        let mut manager = PetgraphGraphManager::new();
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
        // Pool 1: 3 pairs × 2 directions = 6 edges (A-B, B-A, A-C, C-A, B-C, C-B)
        // Pool 2: 1 pair × 2 directions = 2 edges (C-D, D-C)
        // Total: 8 edges
        assert_eq!(graph.edge_count(), 8);

        // Verify edge labels are correct by checking specific token pairs
        let node_a = manager.find_node(&token_a).unwrap();
        let node_b = manager.find_node(&token_b).unwrap();
        let node_c = manager.find_node(&token_c).unwrap();
        let node_d = manager.find_node(&token_d).unwrap();

        // Pool 1 edges: A-B, B-A, A-C, C-A, B-C, C-B (bidirectional)
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_a, node_b).unwrap())
                .unwrap()
                .component_id,
            "pool1".to_string()
        );
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_b, node_a).unwrap())
                .unwrap()
                .component_id,
            "pool1".to_string()
        );
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_a, node_c).unwrap())
                .unwrap()
                .component_id,
            "pool1".to_string()
        );
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_c, node_a).unwrap())
                .unwrap()
                .component_id,
            "pool1".to_string()
        );
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_b, node_c).unwrap())
                .unwrap()
                .component_id,
            "pool1".to_string()
        );
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_c, node_b).unwrap())
                .unwrap()
                .component_id,
            "pool1".to_string()
        );

        // Pool 2 edges: C-D, D-C (bidirectional)
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_c, node_d).unwrap())
                .unwrap()
                .component_id,
            "pool2".to_string()
        );
        assert_eq!(
            graph
                .edge_weight(graph.find_edge(node_d, node_c).unwrap())
                .unwrap()
                .component_id,
            "pool2".to_string()
        );
    }

    #[test]
    fn test_initialize_graph_multiple_edges_same_pair() {
        let mut manager = PetgraphGraphManager::new();
        let mut topology = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        // Multiple components connecting the same token pair
        topology.insert("pool1".to_string(), vec![token_a.clone(), token_b.clone()]);
        topology.insert("pool2".to_string(), vec![token_a.clone(), token_b.clone()]);
        topology.insert("pool3".to_string(), vec![token_a.clone(), token_b.clone()]);

        manager.initialize_graph(&topology);

        let graph = manager.graph();
        // 2 unique tokens
        assert_eq!(graph.node_count(), 2);
        // 3 components × 1 pair × 2 directions = 6 edges between A-B
        assert_eq!(graph.edge_count(), 6);

        let node_a = manager.find_node(&token_a).unwrap();
        let node_b = manager.find_node(&token_b).unwrap();

        // Verify all three edges exist with correct component IDs
        let edges: Vec<_> = graph
            .edges_connecting(node_a, node_b)
            .collect();
        assert_eq!(edges.len(), 3);

        let component_ids: Vec<_> = edges
            .iter()
            .map(|e| &e.weight().component_id)
            .collect();

        // Verify all three component IDs are present
        assert!(component_ids.contains(&&"pool1".to_string()));
        assert!(component_ids.contains(&&"pool2".to_string()));
        assert!(component_ids.contains(&&"pool3".to_string()));
    }
}
