//! The routing graph plus the lookup structures maintained alongside it.
//!
//! Algorithms receive a [`RoutingGraph`] instead of a bare [`StableDiGraph`] so that per-order
//! work which only depends on the graph — resolving a token address to its node, sizing per-node
//! arrays — is a lookup rather than a scan over every node.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
    ops::Deref,
    sync::{Arc, Mutex, PoisonError},
};

use petgraph::{
    graph::{EdgeIndex, NodeIndex},
    visit::EdgeRef,
};
use tycho_simulation::tycho_common::models::Address;

use super::{
    generation_cache::GenerationCache,
    petgraph::{EdgeData, StableDiGraph},
};
use crate::types::ComponentId;

/// The part of a graph reachable from one node within a hop budget.
pub struct Subgraph {
    /// Outgoing edges of every expanded node, as `(target, component)` pairs.
    pub adj: HashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>>,
    /// Every node touched, the source included.
    pub token_nodes: HashSet<NodeIndex>,
    /// Every component backing an edge in `adj`.
    pub component_ids: HashSet<ComponentId>,
}

/// A [`StableDiGraph`] together with the token-to-node index maintained as nodes are inserted.
///
/// Derefs to the underlying graph for read-only traversal. All mutation goes through the inherent
/// methods, which keep the derived structures in sync; there is deliberately no `DerefMut`.
pub struct RoutingGraph<D> {
    graph: StableDiGraph<D>,
    node_map: HashMap<Address, NodeIndex>,
    node_index_bound: usize,
    generation: u64,
    /// Tokens ranked by out-edge degree, keyed by how many were asked for. Rebuilt lazily
    /// whenever the generation moves on; see
    /// [`most_connected_tokens`](Self::most_connected_tokens).
    most_connected: Mutex<GenerationCache<usize, Arc<Vec<Address>>>>,
    /// Subgraphs keyed by `(source, max_hops)`; see
    /// [`reachable_subgraph`](Self::reachable_subgraph).
    subgraphs: Mutex<GenerationCache<(NodeIndex, usize), Arc<Subgraph>>>,
}

impl<D> RoutingGraph<D> {
    /// Creates an empty routing graph.
    pub fn new() -> Self {
        Self {
            graph: StableDiGraph::default(),
            node_map: HashMap::new(),
            node_index_bound: 0,
            generation: 0,
            most_connected: Mutex::new(GenerationCache::new()),
            subgraphs: Mutex::new(GenerationCache::new()),
        }
    }

    /// Returns a counter that changes whenever the graph's topology changes.
    ///
    /// Anything derived purely from the topology stays valid as long as this is unchanged.
    /// Edge weight updates do not bump it: they carry no topology, and callers that depend on
    /// weights read them through the graph rather than caching them.
    pub fn generation(&self) -> u64 {
        self.generation
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

    /// Returns the `count` tokens with the most outgoing edges, highest degree first.
    ///
    /// The ranking is a pure function of the topology, so it is computed once per generation and
    /// shared by every caller until the graph changes.
    pub(crate) fn most_connected_tokens(&self, count: usize) -> Arc<Vec<Address>> {
        // The cache holds only memoised values, so a panic while building one cannot leave it
        // logically inconsistent — recover from poisoning rather than failing every later solve.
        let mut cache = self
            .most_connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.get_or_insert_with(self.generation, count, || {
            let mut by_degree: Vec<(NodeIndex, usize)> = self
                .graph
                .node_indices()
                .map(|node| (node, self.graph.edges(node).count()))
                .collect();
            by_degree.sort_unstable_by_key(|(_, degree)| Reverse(*degree));
            Arc::new(
                by_degree
                    .into_iter()
                    .take(count)
                    .map(|(node, _)| self.graph[node].clone())
                    .collect(),
            )
        })
    }

    /// Returns the subgraph reachable from `source` within `max_hops`, memoised per generation.
    ///
    /// Expansion stops at `max_hops`: nodes discovered at that depth are recorded but their own
    /// edges are not, so the result depends only on the topology, the source and the budget.
    pub(crate) fn reachable_subgraph(&self, source: NodeIndex, max_hops: usize) -> Arc<Subgraph> {
        // See most_connected_tokens on why poisoning is recovered from rather than propagated.
        let mut cache = self
            .subgraphs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.get_or_insert_with(self.generation, (source, max_hops), || {
            Arc::new(self.build_reachable_subgraph(source, max_hops))
        })
    }

    fn build_reachable_subgraph(&self, source: NodeIndex, max_hops: usize) -> Subgraph {
        let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>> = HashMap::new();
        let mut token_nodes: HashSet<NodeIndex> = HashSet::new();
        let mut component_ids: HashSet<ComponentId> = HashSet::new();
        let mut visited_nodes = HashSet::new();
        let mut queued_nodes = VecDeque::new();

        visited_nodes.insert(source);
        token_nodes.insert(source);
        queued_nodes.push_back((source, 0usize));

        while let Some((node, depth)) = queued_nodes.pop_front() {
            if depth >= max_hops {
                continue;
            }
            for edge in self.graph.edges(node) {
                let target = edge.target();
                let component_id = edge.weight().component_id.clone();

                adj.entry(node)
                    .or_default()
                    .push((target, component_id.clone()));
                component_ids.insert(component_id);
                token_nodes.insert(target);

                if visited_nodes.insert(target) {
                    queued_nodes.push_back((target, depth + 1));
                }
            }
        }

        Subgraph { adj, token_nodes, component_ids }
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
        self.generation += 1;
        node
    }

    /// Adds an edge carrying `data` between two existing nodes.
    pub(crate) fn insert_edge(
        &mut self,
        from: NodeIndex,
        to: NodeIndex,
        data: EdgeData<D>,
    ) -> EdgeIndex {
        self.generation += 1;
        self.graph.add_edge(from, to, data)
    }

    /// Removes an edge. Node indices are unaffected — the graph is stable.
    pub(crate) fn remove_edge(&mut self, edge: EdgeIndex) {
        self.generation += 1;
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
        self.generation += 1;
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
    use ::petgraph::visit::EdgeRef;

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
    fn test_generation_advances_on_topology_change() {
        let mut graph = RoutingGraph::<()>::new();
        let start = graph.generation();

        let from = graph.insert_node(&addr(1));
        let after_node = graph.generation();
        assert_ne!(after_node, start, "inserting a node must advance the generation");

        let to = graph.insert_node(&addr(2));
        let edge = graph.insert_edge(from, to, EdgeData::new("pool1".to_string()));
        let after_edge = graph.generation();
        assert_ne!(after_edge, after_node, "inserting an edge must advance the generation");

        graph.remove_edge(edge);
        let after_removal = graph.generation();
        assert_ne!(after_removal, after_edge, "removing an edge must advance the generation");

        graph.clear();
        assert_ne!(graph.generation(), after_removal, "clearing must advance the generation");
    }

    #[test]
    fn test_generation_unchanged_by_edge_data_update() {
        let mut graph = RoutingGraph::<u8>::new();
        let from = graph.insert_node(&addr(1));
        let to = graph.insert_node(&addr(2));
        let edge = graph.insert_edge(from, to, EdgeData::new("pool1".to_string()));

        let before = graph.generation();
        graph
            .edge_data_mut(edge)
            .expect("edge exists")
            .data = Some(7);

        assert_eq!(graph.generation(), before);
    }

    #[test]
    fn test_insert_node_does_not_advance_generation_when_present() {
        let mut graph = RoutingGraph::<()>::new();
        graph.insert_node(&addr(1));

        let before = graph.generation();
        graph.insert_node(&addr(1));

        assert_eq!(graph.generation(), before);
    }

    /// Builds the degree ranking from scratch, the way the memo's callers would without it.
    fn ranked_by_degree(graph: &RoutingGraph<()>, count: usize) -> Vec<Address> {
        let mut by_degree: Vec<(NodeIndex, usize)> = graph
            .node_indices()
            .map(|node| (node, graph.edges(node).count()))
            .collect();
        by_degree.sort_unstable_by_key(|(_, degree)| Reverse(*degree));
        by_degree
            .into_iter()
            .take(count)
            .map(|(node, _)| graph[node].clone())
            .collect()
    }

    /// Hub token (`addr(1)`) with `spokes` outgoing edges, plus one edge between two leaves.
    fn hub_graph(spokes: u8) -> RoutingGraph<()> {
        let mut graph = RoutingGraph::<()>::new();
        let hub = graph.insert_node(&addr(1));
        for spoke in 0..spokes {
            let leaf = graph.insert_node(&addr(10 + spoke));
            graph.insert_edge(hub, leaf, EdgeData::new(format!("hub_{spoke}")));
            graph.insert_edge(leaf, hub, EdgeData::new(format!("hub_{spoke}")));
        }
        graph
    }

    #[test]
    fn test_most_connected_tokens_matches_a_fresh_ranking() {
        let graph = hub_graph(4);

        assert_eq!(*graph.most_connected_tokens(3), ranked_by_degree(&graph, 3));
    }

    #[test]
    fn test_most_connected_tokens_repeated_access_is_stable() {
        let graph = hub_graph(4);

        let first = graph.most_connected_tokens(3);
        let second = graph.most_connected_tokens(3);

        assert_eq!(first, second);
        assert!(Arc::ptr_eq(&first, &second), "a repeat access must not rebuild the ranking");
    }

    #[test]
    fn test_most_connected_tokens_separate_counts_do_not_collide() {
        let graph = hub_graph(4);

        assert_eq!(graph.most_connected_tokens(1).len(), 1);
        assert_eq!(graph.most_connected_tokens(3).len(), 3);
    }

    #[test]
    fn test_most_connected_tokens_reflects_added_edges() {
        let mut graph = hub_graph(2);
        let hub = graph
            .node_index(&addr(1))
            .expect("hub exists");
        assert_eq!(graph.most_connected_tokens(1)[0], addr(1));

        // Give a leaf more edges than the hub, so the top of the ranking has to change.
        let leaf = graph
            .node_index(&addr(10))
            .expect("leaf exists");
        for extra in 0..4u8 {
            let other = graph.insert_node(&addr(100 + extra));
            graph.insert_edge(leaf, other, EdgeData::new(format!("leaf_{extra}")));
        }

        assert_eq!(graph.most_connected_tokens(1)[0], addr(10));
        assert_eq!(*graph.most_connected_tokens(2), ranked_by_degree(&graph, 2));
        assert!(graph.node_index(&addr(1)) == Some(hub), "node indices are stable across inserts");
    }

    #[test]
    fn test_most_connected_tokens_reflects_removed_edges() {
        let mut graph = hub_graph(3);
        assert_eq!(graph.most_connected_tokens(1)[0], addr(1));

        let hub = graph
            .node_index(&addr(1))
            .expect("hub exists");
        let outgoing: Vec<EdgeIndex> = graph
            .edges(hub)
            .map(|edge| edge.id())
            .collect();
        for edge in outgoing {
            graph.remove_edge(edge);
        }

        assert_eq!(*graph.most_connected_tokens(4), ranked_by_degree(&graph, 4));
        assert_ne!(graph.most_connected_tokens(1)[0], addr(1));
    }

    /// Flattens a subgraph's adjacency into a comparable, order-independent form.
    fn adjacency_pairs(subgraph: &Subgraph) -> Vec<(usize, usize, ComponentId)> {
        let mut pairs: Vec<(usize, usize, ComponentId)> = subgraph
            .adj
            .iter()
            .flat_map(|(source, targets)| {
                targets
                    .iter()
                    .map(|(target, component)| (source.index(), target.index(), component.clone()))
            })
            .collect();
        pairs.sort();
        pairs
    }

    #[test]
    fn test_reachable_subgraph_respects_the_hop_budget() {
        // a -> b -> c, so a one-hop budget expands `a` only and never records `b`'s edge.
        let mut graph = RoutingGraph::<()>::new();
        let a = graph.insert_node(&addr(1));
        let b = graph.insert_node(&addr(2));
        let c = graph.insert_node(&addr(3));
        graph.insert_edge(a, b, EdgeData::new("ab".to_string()));
        graph.insert_edge(b, c, EdgeData::new("bc".to_string()));

        let one_hop = graph.reachable_subgraph(a, 1);
        assert_eq!(adjacency_pairs(&one_hop), vec![(a.index(), b.index(), "ab".to_string())]);
        assert_eq!(one_hop.token_nodes, HashSet::from([a, b]));
        assert_eq!(one_hop.component_ids, HashSet::from(["ab".to_string()]));

        let two_hops = graph.reachable_subgraph(a, 2);
        assert_eq!(
            adjacency_pairs(&two_hops),
            vec![
                (a.index(), b.index(), "ab".to_string()),
                (b.index(), c.index(), "bc".to_string())
            ]
        );
        assert_eq!(two_hops.token_nodes, HashSet::from([a, b, c]));
    }

    #[test]
    fn test_reachable_subgraph_repeated_access_is_stable() {
        let graph = hub_graph(3);
        let hub = graph
            .node_index(&addr(1))
            .expect("hub exists");

        let first = graph.reachable_subgraph(hub, 2);
        let second = graph.reachable_subgraph(hub, 2);

        assert!(Arc::ptr_eq(&first, &second), "a repeat access must not rebuild the subgraph");
    }

    #[test]
    fn test_reachable_subgraph_separate_sources_and_budgets_do_not_collide() {
        let graph = hub_graph(3);
        let hub = graph
            .node_index(&addr(1))
            .expect("hub exists");
        let leaf = graph
            .node_index(&addr(10))
            .expect("leaf exists");

        assert_eq!(
            graph
                .reachable_subgraph(hub, 1)
                .adj
                .len(),
            1
        );
        assert_eq!(
            graph
                .reachable_subgraph(hub, 2)
                .adj
                .len(),
            4
        );
        assert_eq!(
            graph
                .reachable_subgraph(leaf, 1)
                .adj
                .len(),
            1
        );
        assert_eq!(
            graph
                .reachable_subgraph(hub, 1)
                .adj
                .len(),
            1
        );
    }

    #[test]
    fn test_reachable_subgraph_reflects_an_added_edge() {
        let mut graph = hub_graph(2);
        let hub = graph
            .node_index(&addr(1))
            .expect("hub exists");
        let before = graph.reachable_subgraph(hub, 1);
        assert!(!before.component_ids.contains("late"));

        let late = graph.insert_node(&addr(200));
        graph.insert_edge(hub, late, EdgeData::new("late".to_string()));

        let after = graph.reachable_subgraph(hub, 1);
        assert!(after.component_ids.contains("late"), "a new edge must appear in the subgraph");
        assert!(after.token_nodes.contains(&late));
        assert_eq!(
            adjacency_pairs(&after),
            adjacency_pairs(&graph.build_reachable_subgraph(hub, 1))
        );
    }

    #[test]
    fn test_reachable_subgraph_reflects_a_removed_edge() {
        let mut graph = hub_graph(2);
        let hub = graph
            .node_index(&addr(1))
            .expect("hub exists");
        assert_eq!(
            graph
                .reachable_subgraph(hub, 1)
                .component_ids
                .len(),
            2
        );

        let doomed = graph
            .edges(hub)
            .map(|edge| edge.id())
            .next()
            .expect("hub has edges");
        graph.remove_edge(doomed);

        let after = graph.reachable_subgraph(hub, 1);
        assert_eq!(after.component_ids.len(), 1, "a removed edge must leave the subgraph");
        assert_eq!(
            adjacency_pairs(&after),
            adjacency_pairs(&graph.build_reachable_subgraph(hub, 1))
        );
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
