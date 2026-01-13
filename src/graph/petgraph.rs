//! Petgraph's StableDiGraph implementation of GraphManager.
//!
//! This module provides PetgraphStableDiGraphManager, which implements GraphManager for
//! petgraph::stable_graph::StableDiGraph, providing a reusable implementation for algorithms that
//! use petgraph.
//!
//! A stable graph is a graph that maintains the indices of its edges even after removals. This is
//! useful for optimising the graph manager's performance by allowing for O(1) edge and node
//! lookups.

use std::collections::{HashMap, HashSet};

use petgraph::{
    graph::{EdgeIndex, NodeIndex},
    stable_graph::StableDiGraph,
};
use tycho_simulation::tycho_core::models::Address;

use super::GraphManager;
use crate::{
    feed::events::{EventError, MarketEvent, MarketEventHandler},
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
    /// The weight of this edge. None if weight has not been set yet.
    pub weight: Option<EdgeWeight>,
}

impl EdgeData {
    /// Creates a new EdgeData with the given component ID and no weight set.
    pub fn new(component_id: ComponentId) -> Self {
        Self { component_id, weight: None }
    }

    /// Extracts the numeric weight value for use in algorithms.
    /// Returns None if weight has not been set, otherwise returns Some(f64).
    pub fn weight_as_f64(&self) -> Option<f64> {
        self.weight.as_ref().map(|w| w.as_f64())
    }
}

/// Petgraph implementation of GraphManager.
///
/// This struct implements GraphManager for petgraph::stable_graph::StableDiGraph.
///
/// The graph manager maintains the graph internally and updates it based on market events.
/// Using StableDiGraph ensures edge indices remain valid after removals, making edge_map viable.
pub struct PetgraphStableDiGraphManager {
    // Stable directed graph with token addresses as nodes and edge data (component id + weight) as
    // edges. Using StableDiGraph ensures edge indices remain valid after removals, making
    // edge_map viable.
    graph: StableDiGraph<Address, EdgeData>,
    // Map from ComponentId to edge indices for fast removal and weight updates.
    edge_map: HashMap<ComponentId, Vec<EdgeIndex>>,
    // Map from token address to node index for fast node lookups.
    node_map: HashMap<Address, NodeIndex>,
}

impl PetgraphStableDiGraphManager {
    pub fn new() -> Self {
        Self { graph: StableDiGraph::default(), edge_map: HashMap::new(), node_map: HashMap::new() }
    }

    /// Helper function to find a node index by address
    fn find_node(&self, addr: &Address) -> Result<NodeIndex, GraphError> {
        self.node_map
            .get(addr)
            .copied()
            .ok_or_else(|| GraphError::TokenNotFound(addr.clone()))
    }

    /// Helper function to get or create a node for the given address.
    /// Returns the node index, creating the node if it doesn't exist.
    fn get_or_create_node(&mut self, addr: &Address) -> NodeIndex {
        // Check if node already exists
        match self.find_node(addr) {
            Ok(node_idx) => node_idx,
            Err(_) => {
                let node_idx = self.graph.add_node(addr.clone());
                self.node_map
                    .insert(addr.clone(), node_idx);
                node_idx
            }
        }
    }

    /// Helper function to add an edge to the graph.
    ///
    /// # Arguments
    ///
    /// * `from_idx` - The index of the from node.
    /// * `to_idx` - The index of the to node.
    /// * `component_id` - The ID of the component represented by this edge.
    fn add_edge(&mut self, from_idx: NodeIndex, to_idx: NodeIndex, component_id: &ComponentId) {
        let edge_idx = self
            .graph
            .add_edge(from_idx, to_idx, EdgeData::new(component_id.clone()));
        self.edge_map
            .entry(component_id.clone())
            .or_default()
            .push(edge_idx);
    }

    /// Helper function to add edges for all token pairs in a component.
    /// Takes a slice of node indices corresponding to the tokens.
    fn add_component_edges(&mut self, component_id: &ComponentId, node_indices: &[NodeIndex]) {
        // Special case: if only 1 node, create a self-loop edge
        if node_indices.len() == 1 {
            let node_idx = node_indices[0];
            self.add_edge(node_idx, node_idx, component_id);
            return;
        }

        // Create bidirectional edges for each token pair
        for (i, &from_idx) in node_indices.iter().enumerate() {
            for &to_idx in node_indices.iter().skip(i + 1) {
                // Create edge A -> B
                self.add_edge(from_idx, to_idx, component_id);

                // Create edge B -> A
                self.add_edge(to_idx, from_idx, component_id);
            }
        }
    }

    /// Adds components to the graph.
    ///
    /// # Errors
    ///
    /// Returns an error if any components have no tokens (components must have at least 1 token).
    /// All components not included in the error were successfully added.
    ///
    /// Arguments:
    /// - components: A map of component IDs to their tokens.
    fn add_components(
        &mut self,
        components: &HashMap<ComponentId, Vec<Address>>,
    ) -> Result<(), GraphError> {
        let mut tokenless_components = Vec::new();

        for (comp_id, tokens) in components {
            if tokens.is_empty() {
                tokenless_components.push(comp_id.clone());
                continue;
            }
            // Ensure all tokens are added as nodes (or get existing ones) and collect their indices
            let node_indices: Vec<NodeIndex> = tokens
                .iter()
                .map(|token| self.get_or_create_node(token))
                .collect();
            // Add edges for all token pairs in this component
            self.add_component_edges(comp_id, &node_indices);
        }

        // Return error if any components had no tokens
        if !tokenless_components.is_empty() {
            return Err(GraphError::ComponentsWithoutTokens(tokenless_components));
        }

        Ok(())
    }

    /// Removes components from the graph.
    ///
    /// # Errors
    ///
    /// Returns an error if any components are not found in the graph. All components not included
    /// in the error were successfully removed.
    ///
    /// Arguments:
    /// - components: A vector of component IDs to remove.
    fn remove_components(&mut self, components: &[ComponentId]) -> Result<(), GraphError> {
        let mut missing_components = Vec::new();

        for comp_id in components {
            // Use the edge_map for O(1) lookup instead of iterating all edges
            if let Some(edge_indices) = self.edge_map.remove(comp_id) {
                for edge_idx in edge_indices {
                    self.graph.remove_edge(edge_idx);
                }
            } else {
                // Component not found in edge_map
                missing_components.push(comp_id.clone());
            }
        }

        // Return error if any components were not found
        if !missing_components.is_empty() {
            return Err(GraphError::ComponentsNotFound(missing_components));
        }

        Ok(())
    }

    /// Sets the weight for edges between the specified tokens with the given component ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the component is not found in the graph for the given token pair.
    ///
    /// Arguments:
    /// - component_id: The ID of the component to update.
    /// - token_in: The input token.
    /// - token_out: The output token.
    /// - weight: The weight to set.
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
        let edge_indices = self
            .edge_map
            .get(component_id)
            .ok_or_else(|| GraphError::ComponentsNotFound(vec![component_id.clone()]))?;

        let mut updated = false;
        for &edge_idx in edge_indices {
            // Skip if edge endpoints are not found
            let (edge_from, edge_to) = match self.graph.edge_endpoints(edge_idx) {
                Some(endpoints) => endpoints,
                None => continue,
            };

            // Determine if we should update this edge based on edge tokens and bidirectional flag
            let should_update = if bidirectional {
                // Update both directions
                (edge_from == from_idx && edge_to == to_idx) ||
                    (edge_from == to_idx && edge_to == from_idx)
            } else {
                // Update only forward direction
                edge_from == from_idx && edge_to == to_idx
            };

            if should_update {
                // Error if edge weight is not found
                let edge_data = self
                    .graph
                    .edge_weight_mut(edge_idx)
                    .ok_or_else(|| GraphError::ComponentsNotFound(vec![component_id.clone()]))?;
                // Verify the component ID matches
                if edge_data.component_id == *component_id {
                    edge_data.weight = Some(weight.clone());
                    updated = true;
                }
            }
        }

        if !updated {
            return Err(GraphError::MissingComponentBetweenTokens(
                token_in.clone(),
                token_out.clone(),
                component_id.clone(),
            ));
        }

        Ok(())
    }
}

impl Default for PetgraphStableDiGraphManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphManager<StableDiGraph<Address, EdgeData>> for PetgraphStableDiGraphManager {
    fn initialize_graph(&mut self, component_topology: &HashMap<ComponentId, Vec<Address>>) {
        // Clear existing graph and component map
        self.graph = StableDiGraph::default();
        self.edge_map.clear();
        self.node_map.clear();

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

    fn graph(&self) -> &StableDiGraph<Address, EdgeData> {
        &self.graph
    }
}

impl MarketEventHandler for PetgraphStableDiGraphManager {
    fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError> {
        match event {
            MarketEvent::MarketUpdated { added_components, removed_components, .. } => {
                // Process both operations and collect all errors
                let mut errors = Vec::new();

                // Try to add components, collect error if it fails
                if let Err(e) = self.add_components(added_components) {
                    errors.push(e);
                }

                // Try to remove components, collect error if it fails
                if let Err(e) = self.remove_components(removed_components) {
                    errors.push(e);
                }

                // Return errors if any occurred
                match errors.len() {
                    0 => Ok(()),
                    _ => Err(EventError::GraphErrors(errors)),
                }
            }
            MarketEvent::GasPriceUpdated { .. } => Err(EventError::InvalidEvent(
                "Gas price updates cannot be applied to the graph".to_string(),
            )),
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
        let mut manager = PetgraphStableDiGraphManager::new();
        let topology = HashMap::new();

        manager.initialize_graph(&topology);

        let graph = manager.graph();
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_initialize_graph_comprehensive() {
        let mut manager = PetgraphStableDiGraphManager::new();
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
        let mut manager = PetgraphStableDiGraphManager::new();
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
        let mut manager = PetgraphStableDiGraphManager::new();
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

        // Verify initial weight is None (not set yet)
        {
            let graph = manager.graph();
            let edge_ab = graph.find_edge(node_a, node_b).unwrap();
            assert_eq!(
                graph
                    .edge_weight(edge_ab)
                    .unwrap()
                    .weight,
                None
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
            Some(EdgeWeight::SpotPrice(42.5))
        );
        let edge_ba = graph.find_edge(node_b, node_a).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ba)
                .unwrap()
                .weight,
            Some(EdgeWeight::SpotPrice(42.5))
        );

        // Clear weight for next test
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
            Some(EdgeWeight::SpotPrice(100.0))
        );
        // B-A should still have the previous weight (Depth(0.0))
        let edge_ba = graph.find_edge(node_b, node_a).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ba)
                .unwrap()
                .weight,
            Some(EdgeWeight::Depth(0.0))
        );

        // Verify other edges in the same component still have no weight set
        {
            let edge_ac = graph.find_edge(node_a, node_c).unwrap();
            assert_eq!(
                graph
                    .edge_weight(edge_ac)
                    .unwrap()
                    .weight,
                None // Not set yet
            );
            let edge_ca = graph.find_edge(node_c, node_a).unwrap();
            assert_eq!(
                graph
                    .edge_weight(edge_ca)
                    .unwrap()
                    .weight,
                None // Not set yet
            );
        }
    }

    #[test]
    fn test_add_components_no_duplicate_nodes() {
        let mut manager = PetgraphStableDiGraphManager::new();
        let mut components = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        // Add first component with token A and B
        components.insert("pool1".to_string(), vec![token_a.clone(), token_b.clone()]);
        manager
            .add_components(&components)
            .unwrap();

        let initial_node_count = manager.graph().node_count();
        assert_eq!(initial_node_count, 2);

        // Add second component with overlapping token A
        components.clear();
        components.insert("pool2".to_string(), vec![token_a.clone()]);
        manager
            .add_components(&components)
            .unwrap();

        // Should still have only 2 nodes, not 3
        assert_eq!(manager.graph().node_count(), 2, "Should not create duplicate nodes");
    }

    #[test]
    fn test_add_tokenless_components_error() {
        let mut manager = PetgraphStableDiGraphManager::new();
        let mut components = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        // Mix valid and invalid components
        components.insert("pool1".to_string(), vec![token_a.clone(), token_b.clone()]);
        components.insert("pool2".to_string(), vec![]);
        components.insert("pool3".to_string(), vec![]);
        let result = manager.add_components(&components);

        assert!(result.is_err());
        match result.unwrap_err() {
            GraphError::ComponentsWithoutTokens(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&"pool2".to_string()));
                assert!(ids.contains(&"pool3".to_string()));
            }
            _ => panic!("Expected ComponentsWithoutTokens error"),
        }

        // Verify valid component was still added
        assert_eq!(manager.graph().node_count(), 2);
        assert_eq!(manager.graph().edge_count(), 2); // A-B and B-A
    }

    #[test]
    fn test_remove_components_not_found_error() {
        let mut manager = PetgraphStableDiGraphManager::new();
        let mut components = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        // Add components first
        components.insert("pool1".to_string(), vec![token_a.clone(), token_b.clone()]);
        components.insert("pool2".to_string(), vec![token_a.clone()]);
        manager
            .add_components(&components)
            .unwrap();

        // Try to remove mix of existing and non-existing components
        let result = manager.remove_components(&[
            "pool1".to_string(),
            "pool3".to_string(),
            "pool4".to_string(),
        ]);

        assert!(result.is_err());
        match result.unwrap_err() {
            GraphError::ComponentsNotFound(ids) => {
                assert_eq!(ids.len(), 2, "Expected 2 missing components");
                assert!(ids.contains(&"pool3".to_string()));
                assert!(ids.contains(&"pool4".to_string()));
            }
            _ => panic!("Expected ComponentsNotFound error"),
        }

        dbg!(manager.graph());
        dbg!(&manager.graph().edge_indices());
        dbg!(&manager.edge_map);

        // Verify pool1 was removed but pool2 is still there
        // pool2 has a single token, so it creates 1 self-loop edge (A->A)
        let final_edge_count = manager.graph().edge_count();
        assert_eq!(
            final_edge_count, 1,
            "Expected 1 edge after removing pool1, got {}",
            final_edge_count
        );
        assert_eq!(manager.edge_map.len(), 1);
    }

    #[test]
    fn test_set_edge_weight_errors() {
        let mut manager = PetgraphStableDiGraphManager::new();
        let mut topology = HashMap::new();
        let token_a = addr("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let token_b = addr("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let token_c = addr("0x6B175474E89094C44Da98b954EedeAC495271d0F"); // DAI

        // Initialize with pool1 connecting A-B, and pool2 connecting B-C
        topology.insert("pool1".to_string(), vec![token_a.clone(), token_b.clone()]);
        topology.insert("pool2".to_string(), vec![token_b.clone(), token_c.clone()]);
        manager.initialize_graph(&topology);

        // Test 1: Component not found
        let result = manager.set_edge_weight(
            &"pool3".to_string(),
            &token_a,
            &token_b,
            EdgeWeight::SpotPrice(42.5),
            true,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            GraphError::ComponentsNotFound(ids) => {
                assert_eq!(ids, vec!["pool3".to_string()]);
            }
            _ => panic!("Expected ComponentsNotFound error"),
        }

        // Test 2: Token not found
        let non_existent_token = addr("0x0000000000000000000000000000000000000000");
        let result = manager.set_edge_weight(
            &"pool1".to_string(),
            &token_a,
            &non_existent_token, // Non-existent token
            EdgeWeight::SpotPrice(42.5),
            true,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            GraphError::TokenNotFound(found_addr) => {
                assert_eq!(found_addr, non_existent_token);
            }
            _ => panic!("Expected TokenNotFound error"),
        }

        // Test 3: Component doesn't connect the specified tokens
        let result = manager.set_edge_weight(
            &"pool1".to_string(),
            &token_a,
            &token_c, // pool1 doesn't connect A-C, only A-B
            EdgeWeight::SpotPrice(42.5),
            true,
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            GraphError::MissingComponentBetweenTokens(in_token, out_token, comp_id) => {
                assert_eq!(in_token, token_a);
                assert_eq!(out_token, token_c);
                assert_eq!(comp_id, "pool1".to_string());
            }
            _ => panic!("Expected MissingComponentBetweenTokens error"),
        }
    }

    #[test]
    fn test_handle_event_error_invalid_gas_price() {
        let mut manager = PetgraphStableDiGraphManager::new();
        use crate::{
            feed::events::{EventError, MarketEvent},
            types::GasPrice,
        };

        // Try to handle a gas price update event
        let event = MarketEvent::GasPriceUpdated { gas_price: GasPrice::default() };

        let result = manager.handle_event(&event);

        assert!(result.is_err());
        match result.unwrap_err() {
            EventError::InvalidEvent(msg) => {
                assert!(msg.contains("Gas price updates cannot be applied"));
            }
            _ => panic!("Expected InvalidEvent error"),
        }
    }

    #[test]
    fn test_handle_event_propagates_errors() {
        let mut manager = PetgraphStableDiGraphManager::new();
        use std::collections::HashMap;

        use crate::feed::events::{EventError, MarketEvent};

        // Create an event with both add and remove operations that will fail
        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([("pool1".to_string(), vec![])]),
            removed_components: vec!["pool2".to_string()],
            updated_components: vec![],
        };

        let result = manager.handle_event(&event);

        // Should return multiple errors
        assert!(result.is_err());
        match result.unwrap_err() {
            EventError::GraphErrors(errors) => {
                assert_eq!(errors.len(), 2);
                // Check that we have both error types
                let has_add_error = errors
                    .iter()
                    .any(|e| matches!(e, GraphError::ComponentsWithoutTokens(_)));
                let has_remove_error = errors
                    .iter()
                    .any(|e| matches!(e, GraphError::ComponentsNotFound(_)));
                assert!(has_add_error, "Should have ComponentsWithoutTokens error");
                assert!(has_remove_error, "Should have ComponentsNotFound error");
            }
            _ => panic!("Expected GraphErrors with multiple errors"),
        }
    }
}
