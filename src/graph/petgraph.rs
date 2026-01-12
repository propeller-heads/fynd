//! Petgraph's UnGraph implementation of GraphManager.
//!
//! This module provides PetgraphUnGraphManager, which implements GraphManager for
//! petgraph::graph::UnGraph, providing a reusable implementation for algorithms that use petgraph.

use std::collections::{HashMap, HashSet};

use petgraph::graph::{DiGraph, EdgeIndex, NodeIndex};
use tracing::error;
use tycho_simulation::tycho_core::models::Address;

use super::GraphManager;
use crate::{
    feed::events::{MarketEvent, MarketEventHandler},
    graph::GraphError,
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

    /// Helper function to get or create a node for the given address.
    /// Returns the node index, creating the node if it doesn't exist.
    fn get_or_create_node(&mut self, addr: &Address) -> NodeIndex {
        // Check if node already exists
        if let Some(node_idx) = self
            .graph
            .node_indices()
            .find(|&idx| self.graph[idx] == *addr)
        {
            node_idx
        } else {
            // Create new node if it doesn't exist
            self.graph.add_node(addr.clone())
        }
    }

    /// Helper function to add edges for all token pairs in a component.
    /// Takes a slice of node indices corresponding to the tokens.
    fn add_component_edges(&mut self, component_id: &ComponentId, node_indices: &[NodeIndex]) {
        // Create bidirectional edges for each token pair
        for (i, &from_idx) in node_indices.iter().enumerate() {
            for &to_idx in node_indices.iter().skip(i + 1) {
                // Create edge A -> B
                let edge_idx_ab =
                    self.graph
                        .add_edge(from_idx, to_idx, EdgeData::new(component_id.clone()));
                self.edge_map
                    .entry(component_id.clone())
                    .or_default()
                    .push(edge_idx_ab);

                // Create edge B -> A
                let edge_idx_ba =
                    self.graph
                        .add_edge(to_idx, from_idx, EdgeData::new(component_id.clone()));
                self.edge_map
                    .entry(component_id.clone())
                    .or_default()
                    .push(edge_idx_ba);
            }
        }
    }

    fn add_components(&mut self, components: &HashMap<ComponentId, Vec<Address>>) {
        for (comp_id, tokens) in components {
            // Ensure all tokens are added as nodes (or get existing ones) and collect their indices
            let node_indices: Vec<NodeIndex> = tokens
                .iter()
                .map(|token| self.get_or_create_node(token))
                .collect();
            // Add edges for all token pairs in this component
            self.add_component_edges(comp_id, &node_indices);
        }
    }

    fn remove_components(&mut self, components: &[ComponentId]) -> Result<(), GraphError> {
        // Use the edge_map for O(1) lookup instead of iterating all edges
        for comp_id in components {
            if let Some(edge_indices) = self.edge_map.remove(comp_id) {
                for edge_idx in edge_indices {
                    self.graph.remove_edge(edge_idx);
                }
            } else {
                return Err(GraphError::EdgeNotFound(comp_id.clone()));
            }
        }
        Ok(())
    }

    /// Sets the weight for edges between the specified tokens with the given component ID.
    ///
    /// - If `bidirectional` is `true`, updates edges in both directions (token_in -> token_out and
    ///   token_out -> token_in).
    /// - If `bidirectional` is `false`, updates only the forward direction (token_in -> token_out).
    pub fn set_edge_weight(
        &mut self,
        component_id: &ComponentId,
        token_in: &Address,
        token_out: &Address,
        weight: EdgeWeight,
        bidirectional: bool,
    ) -> Result<(), GraphError> {
        let from_idx = self.find_node(token_in)?;
        let to_idx = self.find_node(token_out)?;

        // Get all edges for this component
        if let Some(edge_indices) = self.edge_map.get(component_id) {
            for &edge_idx in edge_indices {
                // Check if this edge connects the specified token pair
                let (edge_from, edge_to) = self
                    .graph
                    .edge_endpoints(edge_idx)
                    .ok_or_else(|| GraphError::EdgeNotFound(component_id.clone()))?;

                // Determine if we should update this edge based on bidirectional flag
                let should_update = if bidirectional {
                    // Update both directions
                    (edge_from == from_idx && edge_to == to_idx) ||
                        (edge_from == to_idx && edge_to == from_idx)
                } else {
                    // Update only forward direction
                    edge_from == from_idx && edge_to == to_idx
                };

                if should_update {
                    if let Some(edge_data) = self.graph.edge_weight_mut(edge_idx) {
                        // Verify the component ID matches
                        if edge_data.component_id == *component_id {
                            edge_data.weight = weight.clone();
                        }
                    }
                }
            }
        }
        Ok(())
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

        // Add edges between all tokens in each component
        for (comp_id, tokens) in component_topology {
            let node_indices: Vec<NodeIndex> = tokens
                .iter()
                .map(|token| node_map[token])
                .collect();
            self.add_component_edges(comp_id, &node_indices);
        }
    }

    fn graph(&self) -> &DiGraph<Address, EdgeData> {
        &self.graph
    }
}

impl MarketEventHandler for PetgraphGraphManager {
    fn handle_event(&mut self, event: &MarketEvent) {
        match event {
            MarketEvent::MarketUpdated { added_components, removed_components, .. } => {
                self.add_components(added_components);
                if let Err(e) = self.remove_components(removed_components) {
                    error!("Error removing components from graph: {:?}", e);
                }
            }
            MarketEvent::GasPriceUpdated { .. } => {
                // ignore gas price updates
            }
        }
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

    #[test]
    fn test_set_edge_weight() {
        let mut manager = PetgraphGraphManager::new();
        let mut topology = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let token_c = addr("0x6B175474E89094C44Da98b954EedeAC495271d0F"); // DAI

        // Pool 1: A-B-C (3-token pool, fully connected)
        topology
            .insert("pool1".to_string(), vec![token_a.clone(), token_b.clone(), token_c.clone()]);

        manager.initialize_graph(&topology);

        // Get node indices first
        let node_a = manager.find_node(&token_a).unwrap();
        let node_b = manager.find_node(&token_b).unwrap();
        let node_c = manager.find_node(&token_c).unwrap();

        // Verify initial weight is Depth(0.0)
        {
            let graph = manager.graph();
            let edge_ab = graph.find_edge(node_a, node_b).unwrap();
            assert_eq!(
                graph
                    .edge_weight(edge_ab)
                    .unwrap()
                    .weight,
                EdgeWeight::Depth(0.0)
            );
        }

        // Test 1: Set weight bidirectionally (affects 2 edges: A-B and B-A)
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a,
                &token_b,
                EdgeWeight::SpotPrice(42.5),
                true, // bidirectional
            )
            .unwrap();

        // Verify A-B and B-A edges have the new weight
        let graph = manager.graph();
        let edge_ab = graph.find_edge(node_a, node_b).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ab)
                .unwrap()
                .weight,
            EdgeWeight::SpotPrice(42.5)
        );
        let edge_ba = graph.find_edge(node_b, node_a).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ba)
                .unwrap()
                .weight,
            EdgeWeight::SpotPrice(42.5)
        );

        // Reset to default for next test
        manager
            .set_edge_weight(&"pool1".to_string(), &token_a, &token_b, EdgeWeight::Depth(0.0), true)
            .unwrap();

        // Test 2: Set weight unidirectionally (affects only A-B, not B-A)
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a,
                &token_b,
                EdgeWeight::SpotPrice(100.0),
                false, // unidirectional
            )
            .unwrap();

        // Verify only A-B edge is updated
        let graph = manager.graph();
        let edge_ab = graph.find_edge(node_a, node_b).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ab)
                .unwrap()
                .weight,
            EdgeWeight::SpotPrice(100.0)
        );
        // B-A should still be default
        let edge_ba = graph.find_edge(node_b, node_a).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ba)
                .unwrap()
                .weight,
            EdgeWeight::Depth(0.0)
        );

        // Verify other edges in the same component still have default weight
        {
            let edge_ac = graph.find_edge(node_a, node_c).unwrap();
            assert_eq!(
                graph
                    .edge_weight(edge_ac)
                    .unwrap()
                    .weight,
                EdgeWeight::Depth(0.0) // Still default
            );
            let edge_ca = graph.find_edge(node_c, node_a).unwrap();
            assert_eq!(
                graph
                    .edge_weight(edge_ca)
                    .unwrap()
                    .weight,
                EdgeWeight::Depth(0.0) // Still default
            );
        }
    }

    #[test]
    fn test_add_components_no_duplicate_nodes() {
        let mut manager = PetgraphGraphManager::new();
        let mut components = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        // Add first component with token A and B
        components.insert("pool1".to_string(), vec![token_a.clone(), token_b.clone()]);
        manager.add_components(&components);

        let initial_node_count = manager.graph().node_count();
        assert_eq!(initial_node_count, 2);

        // Add second component with overlapping token A
        components.clear();
        components.insert("pool2".to_string(), vec![token_a.clone()]);
        manager.add_components(&components);

        // Should still have only 2 nodes, not 3
        assert_eq!(manager.graph().node_count(), 2, "Should not create duplicate nodes");
    }
}
