//! Searching the routing graph.
//!
//! The worker owns the graph; a solve borrows it through [`TokenGraph`] and never writes to it.
//! Nothing here reads the market, so a solve can work out the pools it will touch before taking
//! the market lock.
//!
//! # Two steps, not one walk
//!
//! The graph is a [`TopologyGraph`]: one edge per directed token *pair*, holding that pair's pools.
//! So a search is `paths_between_ix` for the token sequences, then
//! [`TopologyGraph::expand_path`] for one path per pool combination along each — which is what the
//! rest of the decomposition still consumes.
//!
//! Splitting it that way is the point. Enumeration used to walk one edge per pool, so a sequence's
//! cost was the product of its legs' pool counts *inside the walk*; now the walk is over pairs and
//! only the expansion pays that product. The expansion is also the only place a caller can be given
//! token sequences instead, which is where the ranking work is headed.

use std::time::Instant;

use rustc_hash::FxHashSet;
use tracing::debug;
use tycho_simulation::tycho_core::models::Address;

use crate::{
    algorithm::{decomposition::models::DirectPath, most_liquid::DepthAndPrice},
    derived::types::TokenGasPrices,
    graph::{GraphQueryFilter, TopologyGraph},
    types::ComponentId,
};

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
    ///
    /// Checked between token sequences during expansion rather than inside the walk.
    /// [`TopologyGraph::paths_between_ix`] takes no deadline, and expansion is where the cost is —
    /// one sequence stands for the product of its legs' pool counts.
    pub(crate) deadline: Option<Instant>,
    /// Tokens a path may pass *through*, or `None` to allow every token.
    ///
    /// The order's own endpoints are always allowed, so `Some(vec![])` still admits the direct
    /// pools — and nothing longer. `None` rather than an empty list means "unrestricted", matching
    /// [`AllowedTokens::connector_tokens`]: an allowlist that came out empty on a thin graph would
    /// otherwise silently delete every multi-hop route.
    pub(crate) connector_tokens: Option<Vec<Address>>,
}

/// The routing graph for the length of one solve.
pub(crate) struct TokenGraph<'a> {
    graph: &'a TopologyGraph<DepthAndPrice>,
    /// Tokens a search may route through, or `None` for every token.
    ///
    /// Resolved once per solve because [`AllowedTokens`] combines two filters that both need a
    /// pass over the graph's tokens, while [`GraphQueryFilter`] wants the answer as one set.
    /// Endpoints are absent from it deliberately — the filter admits a route's own endpoints
    /// whatever it holds.
    allowed: Option<FxHashSet<Address>>,
}

impl<'a> TokenGraph<'a> {
    /// Records which tokens a search may route through.
    pub(crate) fn new(
        graph: &'a TopologyGraph<DepthAndPrice>,
        allowed_tokens: &AllowedTokens<'_>,
    ) -> Self {
        // The price filter needs both endpoints priced; on a thin market it would otherwise drop
        // every route.
        let filter = if allowed_tokens.prices_cover_endpoints() {
            AllowedTokens { ..*allowed_tokens }
        } else {
            AllowedTokens { prices: None, ..*allowed_tokens }
        };

        // Both filters absent is the common case and needs no set at all.
        if filter.connector_tokens.is_none() && filter.prices.is_none() {
            return Self { graph, allowed: None };
        }

        let allowed = graph
            .node_indices()
            .map(|node| &graph[node])
            .filter(|token| filter.allows(token))
            .cloned()
            .collect();
        Self { graph, allowed: Some(allowed) }
    }

    /// Whether the graph holds this token at all.
    pub(crate) fn contains_token(&self, address: &Address) -> bool {
        self.graph
            .get_token_ix(address)
            .is_some()
    }

    /// Simple paths from `sell` to `buy` within `bounds`.
    ///
    /// Empty when either token is absent from the graph, when nothing connects them, or when
    /// `sell` and `buy` are the same token — closing a cycle would revisit the start node, and the
    /// decomposition has no use for round trips.
    ///
    /// One path per *pool* combination, not per token sequence: the pair edge carries every pool
    /// that trades it, and [`TopologyGraph::expand_path`] writes one path per combination along a
    /// sequence. defibot's `all_simple_edge_paths` over a networkx multigraph does the same
    /// (`defibot/swaps/graph.py:103`).
    ///
    /// Pools come out in the order the pair edge holds them, which is the order components were
    /// added to the graph, and the expansion advances the rightmost leg first. That is a different
    /// order from the depth-first walk this replaced, so which paths a `max_paths` cut keeps moves
    /// — the set of paths does not.
    pub(crate) fn paths_between(
        &self,
        sell: &Address,
        buy: &Address,
        bounds: &SearchBounds,
    ) -> Vec<DirectPath> {
        if sell == buy {
            return Vec::new();
        }

        let filter = GraphQueryFilter {
            min_hops: 1,
            max_hops: bounds.max_hops,
            connector_tokens: self.connectors_for(bounds),
        };
        let Ok(sequences) = self
            .graph
            .paths_between(sell, buy, &filter)
        else {
            // Either endpoint absent from the graph. The caller checks that too and reports it;
            // here it is simply no route.
            return Vec::new();
        };

        let mut found: Vec<DirectPath> = Vec::new();
        let mut truncated = false;
        for sequence in &sequences {
            if found.len() >= bounds.max_paths {
                truncated = true;
                break;
            }
            // Never before the first sequence has been expanded. The walk this replaced read the
            // clock once per `DEADLINE_CHECK_INTERVAL` edges, so an already-elapsed deadline still
            // returned whatever it had found on the way to that first read — which is what lets a
            // zero timeout still produce a reference route.
            if !found.is_empty() &&
                bounds
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                truncated = true;
                break;
            }

            let room = bounds.max_paths - found.len();
            let expanded = self
                .graph
                .expand_path(sequence, Some(room));
            truncated |= expanded.len() == room;
            for path in expanded {
                found.push(DirectPath {
                    tokens: path
                        .tokens
                        .iter()
                        .map(|token| (*token).clone())
                        .collect(),
                    components: path
                        .edge_data
                        .iter()
                        .map(|edge| edge.component_id.clone())
                        .collect(),
                });
            }
        }

        if truncated {
            debug!(
                paths = found.len(),
                sequences = sequences.len(),
                "decomposition path enumeration hit its bound; solving over the paths found so far"
            );
        }
        found
    }

    /// The tokens this query may route through: the solve's allowlist narrowed by the query's own.
    ///
    /// `None` on either side means unrestricted, so the intersection of `None` with a set is that
    /// set. A query passing `Some(vec![])` still admits the direct pools — the filter always allows
    /// a route's endpoints — and nothing longer.
    fn connectors_for(&self, bounds: &SearchBounds) -> Option<FxHashSet<Address>> {
        match (self.allowed.as_ref(), bounds.connector_tokens.as_ref()) {
            (None, None) => None,
            (Some(allowed), None) => Some(allowed.clone()),
            (None, Some(wanted)) => Some(wanted.iter().cloned().collect()),
            (Some(allowed), Some(wanted)) => Some(
                wanted
                    .iter()
                    .filter(|token| allowed.contains(*token))
                    .cloned()
                    .collect(),
            ),
        }
    }

    /// The `limit` tokens with the most pools that `accept` allows, deepest first.
    ///
    /// Degree is counted in pools, not in trading partners, which is the signal
    /// `fynd derive-connector-tokens` ranks by. Ties break on the address, so the same graph always
    /// answers the same way.
    pub(crate) fn highest_degree_tokens(
        &self,
        limit: usize,
        accept: impl Fn(&Address) -> bool,
    ) -> Vec<&'a Address> {
        let mut by_degree: Vec<(usize, &Address)> = Vec::new();
        for node in self.graph.node_indices() {
            let token = &self.graph[node];
            if !accept(token) {
                continue;
            }
            // Pools, not pairs. On this graph an edge is a token pair holding several pools, so
            // counting edges would rank a token on five thin pairs above one on three deep ones.
            let pools: usize = self
                .graph
                .edges(node)
                .map(|edge| edge.weight().pools().len())
                .sum();
            by_degree.push((pools, token));
        }
        by_degree.sort_by(|left, right| right.cmp(left));
        by_degree.truncate(limit);
        by_degree
            .into_iter()
            .map(|(_, address)| address)
            .collect()
    }
}

/// Component ids of every path, for snapshotting the market before the solve.
pub(crate) fn path_to_component_ids(paths: &[DirectPath]) -> FxHashSet<ComponentId> {
    // Deduplicate on the borrowed ids first: a three-hop enumeration carries tens of thousands of
    // legs over a few hundred distinct pools.
    let unique: FxHashSet<&ComponentId> = paths
        .iter()
        .flat_map(|path| &path.components)
        .collect();
    unique.into_iter().cloned().collect()
}
