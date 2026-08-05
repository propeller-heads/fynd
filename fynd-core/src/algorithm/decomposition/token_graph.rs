//! Searching the routing graph.
//!
//! The worker owns the graph; a solve borrows it through [`TokenGraph`] and never writes to it.
//! Nothing here reads the market, so a solve can work out the pools it will touch before taking
//! the market lock.

use std::time::Instant;

use petgraph::{graph::NodeIndex, visit::EdgeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::debug;
use tycho_simulation::tycho_core::models::Address;

use crate::{
    algorithm::most_liquid::DepthAndPrice, derived::types::TokenGasPrices,
    graph::petgraph::StableDiGraph, types::ComponentId,
};

/// A token path through the routing graph together with the pool used at each leg.
///
/// Owns its ids so nothing downstream carries the graph's lifetime.
pub(crate) struct DirectPath {
    /// Token addresses visited; one longer than [`DirectPath::components`].
    pub(crate) tokens: Vec<Address>,
    /// Component traded at each leg.
    pub(crate) components: Vec<ComponentId>,
}

/// Which tokens a search may route through.
///
/// A token is allowed when the operator's allowlist admits it and the derived store prices it.
/// The order's own endpoints are always allowed. The price filter is skipped unless both endpoints
/// are priced, since a thin market would otherwise lose every route.
#[derive(Clone, Copy)]
pub(crate) struct AllowedTokens<'a> {
    /// [`AlgorithmConfig::connector_tokens`](crate::algorithm::AlgorithmConfig::connector_tokens);
    /// `None` admits every token.
    pub(crate) connector_tokens: Option<&'a FxHashSet<Address>>,
    /// Derived prices; `None` skips the price filter.
    pub(crate) prices: Option<&'a TokenGasPrices>,
    /// The order's sell and buy tokens.
    pub(crate) endpoints: [&'a Address; 2],
}

impl AllowedTokens<'_> {
    fn allows(&self, token: &Address) -> bool {
        if self.endpoints.contains(&token) {
            return true;
        }
        let in_allowlist = self
            .connector_tokens
            .is_none_or(|allowed| allowed.contains(token));
        let priced = self
            .prices
            .is_none_or(|prices| prices.contains_key(token));
        in_allowlist && priced
    }

    /// Whether the price filter applies at all.
    fn prices_cover_endpoints(&self) -> bool {
        self.prices.is_some_and(|prices| {
            self.endpoints
                .iter()
                .all(|token| prices.contains_key(*token))
        })
    }
}

/// What bounds a path search: how deep and when to stop.
///
/// Enumeration is exponential in the hop limit. A search stopped by a bound returns a prefix of
/// the path set — every path it kept is whole — so hitting one costs candidate quality, not
/// correctness.
pub(crate) struct SearchBounds {
    /// Longest path to walk, counted in hops.
    pub(crate) max_hops: usize,
    /// Paths to keep before the search stops.
    pub(crate) max_paths: usize,
    /// Instant the search stops at, or `None` to run it to completion.
    ///
    /// The solve clock: the caller passes `start + timeout` so the search cannot eat the budget
    /// the solve and the assembly still need.
    pub(crate) deadline: Option<Instant>,
}

/// The routing graph for the length of one solve.
pub(crate) struct TokenGraph<'a> {
    graph: &'a StableDiGraph<DepthAndPrice>,
    address_to_ix: FxHashMap<&'a Address, NodeIndex>,
    /// Whether each node may be routed through, by node index. Read once per edge expanded, which
    /// is why it is an index rather than a lookup by address.
    allowed: Vec<bool>,
}

impl<'a> TokenGraph<'a> {
    /// Indexes `graph` by token address and records which tokens a search may route through.
    pub(crate) fn new(
        graph: &'a StableDiGraph<DepthAndPrice>,
        allowed_tokens: &AllowedTokens<'_>,
    ) -> Self {
        // The price filter needs both endpoints priced; on a thin market it would otherwise drop
        // every route.
        let filter = if allowed_tokens.prices_cover_endpoints() {
            AllowedTokens { ..*allowed_tokens }
        } else {
            AllowedTokens { prices: None, ..*allowed_tokens }
        };

        let mut address_to_ix = FxHashMap::default();
        address_to_ix.reserve(graph.node_count());
        let mut allowed = vec![
            false;
            graph.node_count().max(
                graph
                    .node_indices()
                    .map(|node| node.index() + 1)
                    .max()
                    .unwrap_or(0)
            )
        ];
        for node in graph.node_indices() {
            let token = &graph[node];
            address_to_ix.insert(token, node);
            allowed[node.index()] = filter.allows(token);
        }
        Self { graph, address_to_ix, allowed }
    }

    /// Whether a search may route through this node.
    fn allows(&self, node: NodeIndex) -> bool {
        self.allowed
            .get(node.index())
            .copied()
            .unwrap_or(false)
    }

    /// Whether the graph holds this token at all.
    pub(crate) fn contains_token(&self, address: &Address) -> bool {
        self.address_to_ix.contains_key(address)
    }

    /// Simple paths from `sell` to `buy` within `bounds`.
    ///
    /// Empty when either token is absent from the graph, when nothing connects them, or when
    /// `sell` and `buy` are the same token — closing a cycle would revisit the start node, and the
    /// decomposition has no use for round trips.
    ///
    /// One path per *pool* combination, not per token sequence: the graph carries one edge per
    /// component, so two pools on the same pair yield two paths. defibot's `all_simple_edge_paths`
    /// over a networkx multigraph does the same (`defibot/swaps/graph.py:103`).
    pub(crate) fn paths_between(
        &self,
        sell: &Address,
        buy: &Address,
        bounds: &SearchBounds,
    ) -> Vec<DirectPath> {
        let (Some(&sell_node), Some(&buy_node)) =
            (self.address_to_ix.get(sell), self.address_to_ix.get(buy))
        else {
            return Vec::new();
        };
        // The walk only checks the nodes it steps onto, so the start token is checked here.
        if !self.allows(sell_node) || !self.allows(buy_node) {
            return Vec::new();
        }

        // Walk from the less connected end: both directions enumerate the same paths, but the
        // branching factor at the first level is the start node's degree. Ties keep the sell side.
        let backwards = self.graph.edges(buy_node).count() < self.graph.edges(sell_node).count();
        let (source, target) =
            if backwards { (buy_node, sell_node) } else { (sell_node, buy_node) };

        let mut search = PathSearch {
            graph: self.graph,
            allowed: &self.allowed,
            target,
            bounds,
            since_clock_check: 0,
            truncated: false,
            tokens: vec![&self.graph[source]],
            components: Vec::new(),
            visited: FxHashSet::from_iter([source]),
            found: Vec::new(),
        };
        search.extend(source);
        if search.truncated {
            debug!(
                paths = search.found.len(),
                "decomposition path enumeration hit its bound; solving over the paths found so far"
            );
        }

        let mut found = search.found;
        if backwards {
            // Everything downstream reads a path as sell token first.
            for path in &mut found {
                path.tokens.reverse();
                path.components.reverse();
            }
        }
        found
    }

    /// The token with the most pools that `accept` allows.
    ///
    /// Ties break on the address, so the same graph always answers the same way.
    pub(crate) fn highest_degree_token(
        &self,
        accept: impl Fn(&Address) -> bool,
    ) -> Option<&'a Address> {
        let mut best: Option<(usize, &Address)> = None;
        for node in self.graph.node_indices() {
            let token = &self.graph[node];
            if !accept(token) {
                continue;
            }
            let degree = self.graph.edges(node).count();
            if best.is_none_or(|(current, address)| (degree, token) > (current, address)) {
                best = Some((degree, token));
            }
        }
        best.map(|(_, address)| address)
    }
}

/// Depth-first enumeration of simple paths between two graph nodes.
struct PathSearch<'a, 'b> {
    graph: &'a StableDiGraph<DepthAndPrice>,
    allowed: &'a [bool],
    target: NodeIndex,
    bounds: &'b SearchBounds,
    /// Edges expanded since the clock was last read, so the deadline costs one `Instant::now` per
    /// [`DEADLINE_CHECK_INTERVAL`] edges rather than one per edge.
    since_clock_check: usize,
    /// Set once either bound stopped the search, so the caller can report the truncation.
    truncated: bool,
    tokens: Vec<&'a Address>,
    components: Vec<&'a ComponentId>,
    /// Nodes on the current partial path. Read once per edge expanded, which makes it the hottest
    /// container in the search, and it is keyed by graph node indices we generate ourselves — so
    /// the default hasher's collision resistance buys nothing here.
    visited: FxHashSet<NodeIndex>,
    found: Vec<DirectPath>,
}

/// Edges expanded between two reads of the wall clock.
///
/// Checking every edge would cost more than the check saves; the overshoot past the deadline is
/// bounded by the time to expand this many edges.
const DEADLINE_CHECK_INTERVAL: usize = 1024;

impl PathSearch<'_, '_> {
    /// Whether either bound has stopped the search.
    ///
    /// Latches `truncated` so a run cut short is distinguishable from one that simply ran out of
    /// graph.
    fn exhausted(&mut self) -> bool {
        if self.truncated {
            return true;
        }
        if self.found.len() >= self.bounds.max_paths {
            self.truncated = true;
            return true;
        }
        self.since_clock_check += 1;
        if self.since_clock_check >= DEADLINE_CHECK_INTERVAL {
            self.since_clock_check = 0;
            if self
                .bounds
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.truncated = true;
                return true;
            }
        }
        false
    }

    /// Extends the current partial path by every outgoing edge of `current`.
    fn extend(&mut self, current: NodeIndex) {
        if self.components.len() >= self.bounds.max_hops {
            return;
        }
        // Copying the shared reference out first keeps the graph borrow independent of the
        // mutable borrows of `self` taken inside the loop.
        let graph = self.graph;
        for edge in graph.edges(current) {
            if self.exhausted() {
                return;
            }
            let next = edge.target();
            if self.visited.contains(&next) {
                continue;
            }
            if !self
                .allowed
                .get(next.index())
                .copied()
                .unwrap_or(false)
            {
                continue;
            }

            self.tokens.push(&graph[next]);
            self.components
                .push(&edge.weight().component_id);

            if next == self.target {
                // The target closes the path; a simple path never leaves and re-enters it.
                self.found.push(DirectPath {
                    tokens: self
                        .tokens
                        .iter()
                        .map(|token| (*token).clone())
                        .collect(),
                    components: self
                        .components
                        .iter()
                        .map(|component| (*component).clone())
                        .collect(),
                });
            } else {
                self.visited.insert(next);
                self.extend(next);
                self.visited.remove(&next);
            }

            self.tokens.pop();
            self.components.pop();
        }
    }
}

/// Component ids of every path, for snapshotting the market before the solve.
pub(crate) fn path_component_ids(paths: &[DirectPath]) -> FxHashSet<ComponentId> {
    // Deduplicate on the borrowed ids first: a three-hop enumeration carries tens of thousands of
    // legs over a few hundred distinct pools.
    let unique: FxHashSet<&ComponentId> = paths
        .iter()
        .flat_map(|path| &path.components)
        .collect();
    unique.into_iter().cloned().collect()
}
