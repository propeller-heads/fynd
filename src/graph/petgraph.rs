//! Petgraph's StableDiGraph implementation of GraphManager.
//!
//! This module provides PetgraphStableDiGraphManager, which implements GraphManager for
//! petgraph::stable_graph::StableDiGraph, providing a reusable implementation for algorithms that
//! use petgraph.
//!
//! A stable graph is a graph that maintains the indices of its edges even after removals. This is
//! useful for optimising the graph manager's performance by allowing for O(1) edge and node
//! lookups.

use std::collections::{HashMap, HashSet, VecDeque};

pub use petgraph::graph::EdgeIndex;
use petgraph::{graph::NodeIndex, stable_graph, visit::EdgeRef};
use tycho_simulation::tycho_common::models::Address;

use super::{GraphManager, Path};
use crate::{
    feed::events::{EventError, MarketEvent, MarketEventHandler},
    graph::GraphError,
    types::ComponentId,
};

/// Edge weight containing spot price, liquidity depth (inertia), and trading fee.
///
/// Used by routing algorithms to estimate expected output and rank paths.
/// All fields are optional to support incremental updates.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeWeight {
    /// Spot price for this swap direction (token_out per token_in).
    pub spot_price: Option<f64>,
    /// Liquidity depth (inertia) - higher values mean less slippage.
    pub depth: Option<f64>,
    /// Trading fee as a fraction (e.g., 0.003 for 0.3%).
    pub fee: Option<f64>,
}

impl EdgeWeight {
    /// Creates a new EdgeWeight with all fields set.
    pub fn new(spot_price: f64, depth: f64, fee: f64) -> Self {
        Self { spot_price: Some(spot_price), depth: Some(depth), fee: Some(fee) }
    }

    /// Builder method to set spot price.
    pub fn with_spot_price(self, spot_price: f64) -> Self {
        Self { spot_price: Some(spot_price), ..self }
    }

    /// Builder method to set depth.
    pub fn with_depth(self, depth: f64) -> Self {
        Self { depth: Some(depth), ..self }
    }

    /// Builder method to set fee.
    pub fn with_fee(self, fee: f64) -> Self {
        Self { fee: Some(fee), ..self }
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
}

pub type StableDiGraph = stable_graph::StableDiGraph<Address, EdgeData>;

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
    graph: StableDiGraph,
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
    pub fn find_node(&self, addr: &Address) -> Result<NodeIndex, GraphError> {
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
        // Create bidirectional edges for each token pair
        node_indices
            .iter()
            .enumerate()
            .flat_map(|(i, &from_idx)| {
                node_indices
                    .iter()
                    .skip(i + 1)
                    .map(move |&to_idx| (from_idx, to_idx))
            })
            .for_each(|(from_idx, to_idx)| {
                // Create bidirectional edges A -> B and B -> A
                self.add_edge(from_idx, to_idx, component_id);
                self.add_edge(to_idx, from_idx, component_id);
            });
    }

    /// Adds components to the graph.
    ///
    /// # Errors
    ///
    /// Returns an error if any components have too few tokens (components must have at least 2
    /// tokens). All components not included in the error were successfully added.
    ///
    /// Arguments:
    /// - components: A map of component IDs to their tokens.
    fn add_components(
        &mut self,
        components: &HashMap<ComponentId, Vec<Address>>,
    ) -> Result<(), GraphError> {
        let mut invalid_components = Vec::new();

        for (comp_id, tokens) in components {
            if tokens.len() < 2 {
                invalid_components.push(comp_id.clone());
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

        // Return error if any components had too few tokens (less than 2)
        if !invalid_components.is_empty() {
            return Err(GraphError::InvalidComponents(invalid_components));
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
            // Skip current edge if not found in graph, continue checking next edge
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
                // Error if edge weight is not found (edge is not in graph)
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

    /// Finds all edge paths between two tokens using BFS (shorter paths first).
    ///
    /// Enumerates all edges, not just nodes - so parallel pools are explored separately.
    /// No visited tracking - paths can revisit nodes/edges within the hop budget.
    ///
    /// # Arguments
    /// * `from` - Source token address
    /// * `to` - Destination token address
    /// * `min_hops` - Minimum number of hops (1 = at least one swap like A→B)
    /// * `max_hops` - Maximum number of hops (2 = up to A→B→C, 3 = up to A→B→C→D)
    ///
    /// # Returns
    /// All paths with hop count in range [min_hops, max_hops], ordered by hop count (BFS).
    ///
    /// # Errors
    /// Returns `InvalidHopRange` if min_hops > max_hops.
    pub fn find_paths(
        &self,
        from: &Address,
        to: &Address,
        min_hops: usize,
        max_hops: usize,
    ) -> Result<Vec<Path>, GraphError> {
        if min_hops > max_hops {
            return Err(GraphError::InvalidHopRange(min_hops, max_hops));
        }

        let from_idx = match self.find_node(from) {
            Ok(idx) => idx,
            Err(_) => return Ok(vec![]),
        };
        let to_idx = match self.find_node(to) {
            Ok(idx) => idx,
            Err(_) => return Ok(vec![]),
        };

        let mut paths = Vec::new();

        // Handle edge case: if from == to and min_hops == 0, include empty path
        if from_idx == to_idx && min_hops == 0 {
            paths.push(Vec::new());
        }

        // BFS queue: (current_node, path_so_far)
        let mut queue: VecDeque<(NodeIndex, Path)> = VecDeque::new();
        queue.push_back((from_idx, Path::default()));

        while let Some((current_node, current_path)) = queue.pop_front() {
            if current_path.len() >= max_hops {
                continue;
            }

            for edge in self.graph.edges(current_node) {
                let next_node = edge.target();

                let mut new_path = current_path.clone();
                new_path.push(edge.id());

                // Record path if we reached destination and meet min_hops requirement
                if next_node == to_idx && new_path.len() >= min_hops {
                    paths.push(new_path.clone());
                }

                // Continue exploring (even past destination for paths like A→B→C→B)
                queue.push_back((next_node, new_path));
            }
        }

        Ok(paths)
    }
}

impl Default for PetgraphStableDiGraphManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphManager<StableDiGraph> for PetgraphStableDiGraphManager {
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
        for token in unique_tokens {
            let node_idx = self.graph.add_node(token.clone());
            self.node_map.insert(token, node_idx);
        }

        // Add edges between all tokens in each component
        for (comp_id, tokens) in component_topology {
            let node_indices: Vec<NodeIndex> = tokens
                .iter()
                .map(|token| self.node_map[token])
                .collect();
            self.add_component_edges(comp_id, &node_indices);
        }
    }

    fn graph(&self) -> &StableDiGraph {
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
        let weight1 = EdgeWeight::default().with_spot_price(42.5);
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a,
                &token_b,
                weight1.clone(),
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
            Some(weight1.clone())
        );
        let edge_ba = graph.find_edge(node_b, node_a).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ba)
                .unwrap()
                .weight,
            Some(weight1)
        );

        // Clear weight for next test
        let clear_weight = EdgeWeight::default().with_depth(0.0);
        manager
            .set_edge_weight(&"pool1".to_string(), &token_a, &token_b, clear_weight.clone(), true)
            .unwrap();

        // Test 2: Set weight unidirectionally (affects only A-B, not B-A)
        let weight2 = EdgeWeight::default().with_spot_price(100.0);
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a,
                &token_b,
                weight2.clone(),
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
            Some(weight2)
        );
        // B-A should still have the previous weight (depth 0.0)
        let edge_ba = graph.find_edge(node_b, node_a).unwrap();
        assert_eq!(
            graph
                .edge_weight(edge_ba)
                .unwrap()
                .weight,
            Some(clear_weight)
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
    fn test_add_components_shared_tokens() {
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
        components.insert("pool2".to_string(), vec![token_a.clone(), token_b.clone()]);
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
            GraphError::InvalidComponents(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&"pool2".to_string()));
                assert!(ids.contains(&"pool3".to_string()));
            }
            _ => panic!("Expected InvalidComponents error"),
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
        components.insert("pool2".to_string(), vec![token_a.clone(), token_b.clone()]);
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

        // Verify only pool2 edges remain
        for edge in manager.graph().edge_indices() {
            assert_eq!(
                manager
                    .graph()
                    .edge_weight(edge)
                    .unwrap()
                    .component_id,
                "pool2".to_string()
            );
        }
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
            EdgeWeight::default().with_spot_price(42.5),
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
            EdgeWeight::default().with_spot_price(42.5),
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
            EdgeWeight::default().with_spot_price(42.5),
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
                    .any(|e| matches!(e, GraphError::InvalidComponents(_)));
                let has_remove_error = errors
                    .iter()
                    .any(|e| matches!(e, GraphError::ComponentsNotFound(_)));
                assert!(has_add_error, "Should have InvalidComponents error");
                assert!(has_remove_error, "Should have ComponentsNotFound error");
            }
            _ => panic!("Expected GraphErrors with multiple errors"),
        }
    }

    // ==================== find_paths tests ====================

    fn a() -> Address {
        addr("0x000000000000000000000000000000000000000A")
    }
    fn b() -> Address {
        addr("0x000000000000000000000000000000000000000B")
    }
    fn c() -> Address {
        addr("0x000000000000000000000000000000000000000C")
    }
    fn d() -> Address {
        addr("0x000000000000000000000000000000000000000D")
    }

    fn linear_graph() -> PetgraphStableDiGraphManager {
        // A <-> B <-> C <-> D (bidirectional)
        let mut m = PetgraphStableDiGraphManager::new();
        let mut t = HashMap::new();
        t.insert("ab".into(), vec![a(), b()]);
        t.insert("bc".into(), vec![b(), c()]);
        t.insert("cd".into(), vec![c(), d()]);
        m.initialize_graph(&t);
        m
    }

    fn parallel_graph() -> PetgraphStableDiGraphManager {
        // 3 pools A<->B, 2 pools B<->C
        let mut m = PetgraphStableDiGraphManager::new();
        let mut t = HashMap::new();
        t.insert("ab1".into(), vec![a(), b()]);
        t.insert("ab2".into(), vec![a(), b()]);
        t.insert("ab3".into(), vec![a(), b()]);
        t.insert("bc1".into(), vec![b(), c()]);
        t.insert("bc2".into(), vec![b(), c()]);
        m.initialize_graph(&t);
        m
    }

    fn diamond_graph() -> PetgraphStableDiGraphManager {
        // A->B->D, A->C->D (two 2-hop paths)
        let mut m = PetgraphStableDiGraphManager::new();
        let mut t = HashMap::new();
        t.insert("ab".into(), vec![a(), b()]);
        t.insert("ac".into(), vec![a(), c()]);
        t.insert("bd".into(), vec![b(), d()]);
        t.insert("cd".into(), vec![c(), d()]);
        m.initialize_graph(&t);
        m
    }

    fn all_ids<'a>(paths: &[Path], graph: &'a StableDiGraph) -> HashSet<Vec<&'a str>> {
        paths
            .iter()
            .map(|p| {
                p.iter()
                    .map(|h| graph[*h].component_id.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_find_paths_linear_forward_and_reverse() {
        let m = linear_graph();

        // Forward: A->B (1 hop), A->C (2 hops), A->D (3 hops)
        let p = m.find_paths(&a(), &b(), 1, 1).unwrap();
        assert_eq!(all_ids(&p, &m.graph), HashSet::from([vec!["ab"]]));

        let p = m.find_paths(&a(), &c(), 1, 2).unwrap();
        assert_eq!(all_ids(&p, &m.graph), HashSet::from([vec!["ab", "bc"]]));

        let p = m.find_paths(&a(), &d(), 1, 3).unwrap();
        assert_eq!(all_ids(&p, &m.graph), HashSet::from([vec!["ab", "bc", "cd"]]));

        // Reverse: D->A (bidirectional pools)
        let p = m.find_paths(&d(), &a(), 1, 3).unwrap();
        assert_eq!(all_ids(&p, &m.graph), HashSet::from([vec!["cd", "bc", "ab"]]));
    }

    #[test]
    fn test_find_paths_respects_hop_bounds() {
        let m = linear_graph();

        // A->D needs 3 hops, max_hops=2 finds nothing
        assert!(m
            .find_paths(&a(), &d(), 1, 2)
            .unwrap()
            .is_empty());

        // A->C is 2 hops, min_hops=3 finds nothing
        assert!(m
            .find_paths(&a(), &c(), 3, 3)
            .unwrap()
            .is_empty());

        // min_hops > max_hops returns error
        assert!(matches!(m.find_paths(&a(), &b(), 5, 3), Err(GraphError::InvalidHopRange(5, 3))));
    }

    #[test]
    fn test_find_paths_parallel_pools() {
        let m = parallel_graph();

        // A->B: 3 parallel pools = 3 paths
        let p = m.find_paths(&a(), &b(), 1, 1).unwrap();
        assert_eq!(all_ids(&p, &m.graph), HashSet::from([vec!["ab1"], vec!["ab2"], vec!["ab3"]]));

        // A->C: 3 A->B pools × 2 B->C pools = 6 paths
        let p = m.find_paths(&a(), &c(), 1, 2).unwrap();
        assert_eq!(
            all_ids(&p, &m.graph),
            HashSet::from([
                vec!["ab1", "bc1"],
                vec!["ab1", "bc2"],
                vec!["ab2", "bc1"],
                vec!["ab2", "bc2"],
                vec!["ab3", "bc1"],
                vec!["ab3", "bc2"],
            ])
        );
    }

    #[test]
    fn test_find_paths_diamond_multiple_routes() {
        let m = diamond_graph();

        // A->D: two 2-hop paths
        let p = m.find_paths(&a(), &d(), 1, 2).unwrap();
        assert_eq!(all_ids(&p, &m.graph), HashSet::from([vec!["ab", "bd"], vec!["ac", "cd"]]));
    }

    #[test]
    fn test_find_paths_revisit_destination() {
        let m = linear_graph();

        // A->B with max_hops=3: finds 1-hop path plus 3-hop revisit paths
        let p = m.find_paths(&a(), &b(), 1, 3).unwrap();

        // Check all expected paths are found (order-independent)
        assert_eq!(
            all_ids(&p, &m.graph),
            HashSet::from([
                vec!["ab"],             // 1-hop direct
                vec!["ab", "ab", "ab"], // 3-hop: revisit via self
                vec!["ab", "bc", "bc"], // 3-hop: A->B->C->B
            ])
        );
    }

    #[test]
    fn test_find_paths_cyclic_same_source_dest() {
        // Use parallel_graph with 3 A<->B pools to verify all combinations
        let m = parallel_graph();

        // A->A (cyclic path) with 2 hops: should find all 9 combinations (3 pools × 3 pools)
        let p = m.find_paths(&a(), &a(), 0, 2).unwrap();
        assert_eq!(
            all_ids(&p, &m.graph),
            HashSet::from([
                vec![],
                vec!["ab1", "ab1"],
                vec!["ab1", "ab2"],
                vec!["ab1", "ab3"],
                vec!["ab2", "ab1"],
                vec!["ab2", "ab2"],
                vec!["ab2", "ab3"],
                vec!["ab3", "ab1"],
                vec!["ab3", "ab2"],
                vec!["ab3", "ab3"],
            ])
        );
    }

    #[test]
    fn test_find_paths_edge_cases() {
        let m = linear_graph();
        let empty = PetgraphStableDiGraphManager::new();
        let non_existent = addr("0x0000000000000000000000000000000000000099");

        // Empty graph
        assert!(empty
            .find_paths(&a(), &b(), 1, 3)
            .unwrap()
            .is_empty());

        // Token not in graph
        assert!(m
            .find_paths(&non_existent, &b(), 1, 3)
            .unwrap()
            .is_empty());
        assert!(m
            .find_paths(&a(), &non_existent, 1, 3)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_find_paths_bfs_ordering() {
        let m = linear_graph();

        // BFS ensures shorter paths come first: 1-hop before 3-hop
        let p = m.find_paths(&a(), &b(), 1, 3).unwrap();

        // Verify BFS property: paths are ordered by hop count
        assert_eq!(p.len(), 3, "Expected 3 paths total");
        assert_eq!(p[0].len(), 1, "First path should be 1-hop");
        assert_eq!(p[1].len(), 3, "Second path should be 3-hop");
        assert_eq!(p[2].len(), 3, "Third path should be 3-hop");
    }
}
