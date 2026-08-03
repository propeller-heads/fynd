//! The routing graph plus the lookup structures maintained alongside it.
//!
//! Algorithms receive a [`RoutingGraph`] instead of a bare [`StableDiGraph`] so that per-order
//! work which only depends on the graph — resolving a token address to its node, sizing per-node
//! arrays — is a lookup rather than a scan over every node.

use std::{collections::HashMap, ops::Deref};

use petgraph::graph::{EdgeIndex, NodeIndex};
use tycho_simulation::tycho_common::models::Address;

use super::petgraph::{EdgeData, StableDiGraph};

/// A [`StableDiGraph`] together with the token-to-node index maintained as nodes are inserted.
///
/// Derefs to the underlying graph for read-only traversal. All mutation goes through the inherent
/// methods, which keep the derived structures in sync; there is deliberately no `DerefMut`.
pub struct RoutingGraph<D> {
    graph: StableDiGraph<D>,
    node_map: HashMap<Address, NodeIndex>,
    node_index_bound: usize,
}

impl<D> RoutingGraph<D> {
    /// Creates an empty routing graph.
    pub fn new() -> Self {
        Self { graph: StableDiGraph::default(), node_map: HashMap::new(), node_index_bound: 0 }
    }

    /// Returns the node holding `address`, or `None` if the token is not in the graph.
    pub fn node_index(&self, address: &Address) -> Option<NodeIndex> {
        self.node_map.get(address).copied()
    }

    /// Returns an exclusive upper bound on the node indices in the graph, i.e. the length a
    /// per-node array must have to be indexable by any [`NodeIndex`] the graph can yield.
    ///
    /// Nodes are only ever added, so this is the number of nodes inserted since the graph was
    /// last cleared.
    pub fn node_index_bound(&self) -> usize {
        self.node_index_bound
    }

    /// Returns the node holding `address`, inserting it if the token is not yet in the graph.
    pub(crate) fn insert_node(&mut self, address: &Address) -> NodeIndex {
        if let Some(node) = self.node_map.get(address) {
            return *node;
        }
        let node = self.graph.add_node(address.clone());
        self.node_map
            .insert(address.clone(), node);
        self.node_index_bound = self
            .node_index_bound
            .max(node.index() + 1);
        node
    }

    /// Adds an edge carrying `data` between two existing nodes.
    pub(crate) fn insert_edge(
        &mut self,
        from: NodeIndex,
        to: NodeIndex,
        data: EdgeData<D>,
    ) -> EdgeIndex {
        self.graph.add_edge(from, to, data)
    }

    /// Removes an edge. Node indices are unaffected — the graph is stable.
    pub(crate) fn remove_edge(&mut self, edge: EdgeIndex) {
        self.graph.remove_edge(edge);
    }

    /// Returns mutable access to an edge's data. Edge data carries no topology, so the lookup
    /// structures stay valid.
    pub(crate) fn edge_data_mut(&mut self, edge: EdgeIndex) -> Option<&mut EdgeData<D>> {
        self.graph.edge_weight_mut(edge)
    }

    /// Drops all nodes and edges.
    pub(crate) fn clear(&mut self) {
        self.graph = StableDiGraph::default();
        self.node_map.clear();
        self.node_index_bound = 0;
    }
}

impl<D> Default for RoutingGraph<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D> Deref for RoutingGraph<D> {
    type Target = StableDiGraph<D>;

    fn deref(&self) -> &Self::Target {
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
    fn test_insert_node_is_idempotent() {
        let mut graph = RoutingGraph::<()>::new();
        let first = graph.insert_node(&addr(1));
        let second = graph.insert_node(&addr(1));

        assert_eq!(first, second);
        assert_eq!(graph.node_count(), 1);
    }

    #[test]
    fn test_node_index_matches_scan() {
        let mut graph = RoutingGraph::<()>::new();
        for byte in 1..=4 {
            graph.insert_node(&addr(byte));
        }

        for byte in 1..=4 {
            let scanned = graph
                .node_indices()
                .find(|&node| graph[node] == addr(byte));
            assert_eq!(graph.node_index(&addr(byte)), scanned);
        }
        assert_eq!(graph.node_index(&addr(9)), None);
    }

    #[test]
    fn test_node_index_bound_covers_every_node() {
        let mut graph = RoutingGraph::<()>::new();
        assert_eq!(graph.node_index_bound(), 0);

        for byte in 1..=5 {
            graph.insert_node(&addr(byte));
        }

        let max_index = graph
            .node_indices()
            .map(|node| node.index())
            .max()
            .expect("graph has nodes");
        assert_eq!(graph.node_index_bound(), max_index + 1);
    }

    #[test]
    fn test_clear_resets_lookups() {
        let mut graph = RoutingGraph::<()>::new();
        graph.insert_node(&addr(1));
        graph.clear();

        assert_eq!(graph.node_index(&addr(1)), None);
        assert_eq!(graph.node_index_bound(), 0);
        assert_eq!(graph.node_count(), 0);
    }
}
