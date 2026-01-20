//! Graph types and manager for brute-force algorithms.
//!
//! This module provides:
//! - `EdgeData<D>`: Data stored on each graph edge
//! - `Path<'a, D>`: A path through the graph
//! - `GraphManager<D>`: Manages graph topology (nodes, edges, lookups)

use std::collections::HashMap;

use petgraph::{
    graph::{EdgeIndex, NodeIndex},
    stable_graph::StableDiGraph,
};
use tycho_simulation::tycho_common::models::Address;

use crate::types::ComponentId;

/// Data stored on each edge of the graph.
///
/// Contains the component ID (which pool this edge represents) and
/// optional algorithm-specific data. The type `D` is generic to allow
/// different scorers to store their own data.
///
/// # Type Parameters
///
/// - `D`: Scorer-specific data type. Defaults to `()` for no extra data.
#[derive(Debug, Clone, Default)]
pub struct EdgeData<D = ()> {
    /// The component ID that enables this swap.
    pub component_id: ComponentId,
    /// Scorer-specific data. None if not yet computed.
    pub data: Option<D>,
}

impl<D> EdgeData<D> {
    /// Creates a new EdgeData with the given component ID and no data set.
    pub fn new(component_id: ComponentId) -> Self {
        Self { component_id, data: None }
    }

    /// Creates a new EdgeData with the given component ID and data.
    pub fn with_data(component_id: ComponentId, data: D) -> Self {
        Self { component_id, data: Some(data) }
    }
}

/// A path through the graph as a sequence of tokens and edges.
///
/// This representation allows O(1) access to edge data during scoring and simulation.
#[derive(Clone, Default)]
pub struct Path<'a, D> {
    /// Sequence of token addresses in the path.
    pub tokens: Vec<&'a Address>,
    /// Sequence of edge data representing the path. Length is tokens.len() - 1.
    pub edge_data: Vec<&'a EdgeData<D>>,
}

impl<'a, D> Path<'a, D> {
    /// Creates a new empty Path.
    pub fn new() -> Self {
        Self { tokens: Vec::new(), edge_data: Vec::new() }
    }

    /// Adds a hop to the path.
    ///
    /// # Arguments
    ///
    /// - `from`: The starting token address of the hop.
    /// - `edge_data`: The edge data for the hop.
    /// - `to`: The ending token address of the hop.
    pub fn add_hop(&mut self, from: &'a Address, edge_data: &'a EdgeData<D>, to: &'a Address) {
        if self.tokens.is_empty() {
            self.tokens.push(from);
        }
        self.tokens.push(to);
        self.edge_data.push(edge_data);
    }

    /// Returns the number of hops in the path.
    pub fn len(&self) -> usize {
        self.edge_data.len()
    }

    /// Returns true if the path has no hops.
    pub fn is_empty(&self) -> bool {
        self.edge_data.is_empty()
    }

    /// Returns a slice of the edges in the path.
    pub fn edge_iter(&self) -> &[&'a EdgeData<D>] {
        &self.edge_data
    }

    /// Returns an iterator over hops in the path (from_token, edge_data, to_token).
    pub fn iter(&self) -> impl Iterator<Item = (&'a Address, &'a EdgeData<D>, &'a Address)> + '_ {
        self.tokens
            .windows(2)
            .zip(self.edge_data.iter())
            .map(|(tokens, edge)| (tokens[0], *edge, tokens[1]))
    }

    /// Returns the starting token.
    pub fn start_token(&self) -> Option<&Address> {
        self.tokens.first().copied()
    }

    /// Returns the ending token.
    pub fn end_token(&self) -> Option<&Address> {
        self.tokens.last().copied()
    }
}

/// Manages the routing graph topology.
///
/// This struct owns the graph and provides methods for:
/// - Adding/removing components (which create bidirectional edges)
/// - Looking up nodes by address
/// - Accessing edge data for weight updates
///
/// # Type Parameters
///
/// - `D`: The edge data type (determined by the scorer)
pub struct GraphManager<D> {
    /// The underlying petgraph directed graph.
    graph: StableDiGraph<Address, EdgeData<D>>,
    /// Maps token addresses to their node indices for O(1) lookup.
    node_map: HashMap<Address, NodeIndex>,
    /// Maps component IDs to their edge indices for removal.
    edge_map: HashMap<ComponentId, Vec<EdgeIndex>>,
}

impl<D: Clone + Default> Default for GraphManager<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: Clone + Default> GraphManager<D> {
    /// Creates a new empty GraphManager.
    pub fn new() -> Self {
        Self { graph: StableDiGraph::new(), node_map: HashMap::new(), edge_map: HashMap::new() }
    }

    /// Initializes the graph from a component topology.
    ///
    /// # Arguments
    ///
    /// * `components` - Map of component IDs to their token addresses
    pub fn initialize(&mut self, components: &HashMap<ComponentId, Vec<Address>>) {
        for (component_id, tokens) in components {
            self.add_component(component_id, tokens);
        }
    }

    /// Adds a component to the graph with bidirectional edges between all token pairs.
    ///
    /// For a component with N tokens, creates N*(N-1) directed edges (all pairs, both directions).
    pub fn add_component(&mut self, component_id: &ComponentId, tokens: &[Address]) {
        if tokens.len() < 2 {
            return; // Skip invalid components
        }

        let node_indices: Vec<NodeIndex> = tokens
            .iter()
            .map(|t| self.get_or_create_node(t))
            .collect();

        let mut edges = Vec::new();
        for (i, &node_a) in node_indices.iter().enumerate() {
            for &node_b in node_indices.iter().skip(i + 1) {
                // Bidirectional edges
                let edge_ab =
                    self.graph
                        .add_edge(node_a, node_b, EdgeData::new(component_id.clone()));
                let edge_ba =
                    self.graph
                        .add_edge(node_b, node_a, EdgeData::new(component_id.clone()));
                edges.extend([edge_ab, edge_ba]);
            }
        }
        self.edge_map
            .insert(component_id.clone(), edges);
    }

    /// Removes a component and all its edges from the graph.
    pub fn remove_component(&mut self, component_id: &ComponentId) {
        if let Some(edges) = self.edge_map.remove(component_id) {
            for edge_idx in edges {
                self.graph.remove_edge(edge_idx);
            }
        }
    }

    /// Gets or creates a node for the given address.
    fn get_or_create_node(&mut self, addr: &Address) -> NodeIndex {
        if let Some(&idx) = self.node_map.get(addr) {
            idx
        } else {
            let idx = self.graph.add_node(addr.clone());
            self.node_map.insert(addr.clone(), idx);
            idx
        }
    }

    /// Returns the node index for an address, if it exists.
    pub fn get_node(&self, addr: &Address) -> Option<NodeIndex> {
        self.node_map.get(addr).copied()
    }

    /// Returns the address at the given node index.
    pub fn node_address(&self, idx: NodeIndex) -> &Address {
        &self.graph[idx]
    }

    /// Returns the edge indices for a component, if it exists.
    pub fn get_component_edges(&self, component_id: &ComponentId) -> Option<&[EdgeIndex]> {
        self.edge_map
            .get(component_id)
            .map(|v| v.as_slice())
    }

    /// Returns the source and target node indices for an edge.
    pub fn edge_endpoints(&self, edge_idx: EdgeIndex) -> Option<(NodeIndex, NodeIndex)> {
        self.graph.edge_endpoints(edge_idx)
    }

    /// Returns a mutable reference to an edge's data.
    pub fn edge_weight_mut(&mut self, edge_idx: EdgeIndex) -> Option<&mut EdgeData<D>> {
        self.graph.edge_weight_mut(edge_idx)
    }

    /// Returns an iterator over outgoing edges from a node.
    pub fn edges(
        &self,
        node: NodeIndex,
    ) -> impl Iterator<Item = petgraph::stable_graph::EdgeReference<'_, EdgeData<D>>> {
        self.graph.edges(node)
    }

    /// Returns the underlying graph for read-only access during path finding.
    ///
    /// This is used internally by the algorithm to traverse the graph.
    pub fn graph(&self) -> &StableDiGraph<Address, EdgeData<D>> {
        &self.graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    #[test]
    fn graph_manager_add_component() {
        let mut manager = GraphManager::<()>::new();
        let a = addr(0x0A);
        let b = addr(0x0B);

        manager.add_component(&"pool1".to_string(), &[a.clone(), b.clone()]);

        assert!(manager.get_node(&a).is_some());
        assert!(manager.get_node(&b).is_some());
        assert!(manager
            .get_component_edges(&"pool1".to_string())
            .is_some());
    }

    #[test]
    fn graph_manager_remove_component() {
        let mut manager = GraphManager::<()>::new();
        let a = addr(0x0A);
        let b = addr(0x0B);

        manager.add_component(&"pool1".to_string(), &[a.clone(), b.clone()]);
        let edges_before = manager
            .get_component_edges(&"pool1".to_string())
            .map(|e| e.len());
        assert_eq!(edges_before, Some(2)); // Bidirectional

        manager.remove_component(&"pool1".to_string());
        assert!(manager
            .get_component_edges(&"pool1".to_string())
            .is_none());
    }

    #[test]
    fn graph_manager_initialize() {
        let mut manager = GraphManager::<()>::new();
        let a = addr(0x0A);
        let b = addr(0x0B);
        let c = addr(0x0C);

        let components = HashMap::from([
            ("pool_ab".to_string(), vec![a.clone(), b.clone()]),
            ("pool_bc".to_string(), vec![b.clone(), c.clone()]),
        ]);

        manager.initialize(&components);

        assert!(manager.get_node(&a).is_some());
        assert!(manager.get_node(&b).is_some());
        assert!(manager.get_node(&c).is_some());
        assert!(manager
            .get_component_edges(&"pool_ab".to_string())
            .is_some());
        assert!(manager
            .get_component_edges(&"pool_bc".to_string())
            .is_some());
    }

    #[test]
    fn graph_manager_skip_invalid_component() {
        let mut manager = GraphManager::<()>::new();
        let a = addr(0x0A);

        // Single token - should be skipped
        manager.add_component(&"invalid".to_string(), std::slice::from_ref(&a));

        assert!(manager.get_node(&a).is_none());
        assert!(manager
            .get_component_edges(&"invalid".to_string())
            .is_none());
    }
}
