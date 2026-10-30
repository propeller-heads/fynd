//! Graph management for algorithms.
//!
//! This module provides the GraphManager trait which solvers use to manage their market graph
//! representation. GraphManager handles both building graphs from market data and updating them
//! based on market events.

pub mod petgraph;
pub mod token_graph;

pub use petgraph::{EdgeData, PetgraphStableDiGraphManager, StableDiGraph};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use thiserror::Error;
pub use token_graph::{PairEdge, TokenGraph, TokenPath, TopologyGraph, TopologyGraphManager};
use tycho_simulation::{
    evm::protocol::native_wrapper::state::NATIVE_WRAPPER_ID,
    tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim},
    tycho_core::models::token::Token,
};

use crate::{
    derived::DerivedData,
    feed::market_data::MarketDataView,
    types::{ComponentId, ComponentTopology, ResolvedRouteConstraints},
};

/// Whether this component is the injected native-to-wrapped swap.
///
/// A wrap is not a trade, so routes do not count it against their hop limits.
pub(crate) fn is_native_wrap(component_id: &ComponentId) -> bool {
    component_id == NATIVE_WRAPPER_ID
}

/// Tokens held without allocating. A path of `h` hops names `h + 1` tokens, so this covers every
/// `max_hops` up to 4. A deeper path still works: `SmallVec` moves to the heap and behaves as a
/// `Vec` from there.
pub(crate) const INLINE_TOKENS: usize = 5;

/// Edges held without allocating. A path's edges are its tokens less one, and an edge is a leg of
/// a route, so this sizes per-leg buffers too.
/// Edges a path holds inline before it spills to the heap. One fewer than its tokens.
pub const INLINE_EDGES: usize = INLINE_TOKENS - 1;

/// A route with a pool chosen for every leg.
///
/// Borrows from the graph rather than copying it, so scoring and simulation read a leg's component
/// id and weight without a lookup.
#[derive(Default)]
pub struct Path<'a, D> {
    /// The tokens the route passes through, in order.
    pub tokens: SmallVec<[&'a Address; INLINE_TOKENS]>,
    /// The pool taken on each leg. One shorter than `tokens`.
    pub edge_data: SmallVec<[&'a EdgeData<D>; INLINE_EDGES]>,
}

/// Written out rather than derived so the copy is a `memcpy`: `SmallVec` takes that path only
/// through `from_slice`, which the derived `Clone` cannot call.
impl<D> Clone for Path<'_, D> {
    fn clone(&self) -> Self {
        Self {
            tokens: SmallVec::from_slice(&self.tokens),
            edge_data: SmallVec::from_slice(&self.edge_data),
        }
    }
}

impl<'a, D> Path<'a, D> {
    /// Creates a new empty Path.
    pub fn new() -> Self {
        Self { tokens: SmallVec::new(), edge_data: SmallVec::new() }
    }

    /// Adds a hop to the path.
    ///
    /// Arguments:
    /// - from: The starting token address of the hop.
    /// - edge_data: The edge data for the hop.
    /// - to: The ending token address of the hop.
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

    /// Returns the number of hops that count against `max_hops`: every edge except the native
    /// wrap, and at least 1 for a route that is only a wrap.
    pub fn pool_hops(&self) -> usize {
        if self.is_empty() {
            return 0;
        }
        self.non_wrap_hops().max(1)
    }

    /// Returns the number of edges that are not the native wrap.
    pub fn non_wrap_hops(&self) -> usize {
        self.edge_data
            .iter()
            .filter(|edge| !is_native_wrap(&edge.component_id))
            .count()
    }

    /// Returns true if the path has no hops.
    pub fn is_empty(&self) -> bool {
        self.edge_data.is_empty()
    }

    /// Returns an iterator over the edges in the path.
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
}

/// Errors that can occur during graph operations.
#[derive(Error, Debug)]
pub enum GraphError {
    /// Token address not found as a node in the graph.
    #[error("Token not found in graph: {0:?}")]
    TokenNotFound(Address),
    /// One or more components were not found in the graph.
    #[error("Components not found in graph: {0:?}")]
    ComponentsNotFound(Vec<ComponentId>),
    /// Components with fewer than 2 tokens cannot form edges.
    #[error("Components with less then 2 tokens cannot be added: {0:?}")]
    InvalidComponents(Vec<ComponentId>),
    /// No edge exists between the given tokens for this component (test-only).
    #[cfg(any(test, feature = "test-utils"))]
    #[error("No edge found between tokens {0:?} and {1:?} for component {2}")]
    MissingComponentBetweenTokens(Address, Address, ComponentId),
}

/// Trait for managing graph representations.
///
/// Graph managers are stateful - they maintain the graph internally and update it based on market
/// events.
pub trait GraphManager<G>: Send + Sync
where
    G: Send + Sync,
{
    /// Initializes the graph from the market topology.
    ///
    /// Arguments:
    /// - components: A map of component IDs to their tokens and swap directions.
    fn initialize_graph(&mut self, components: &FxHashMap<ComponentId, ComponentTopology>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;
}

/// The bounds a route search runs under.
///
/// Bundles what an algorithm configures once — how long a route may be, and which tokens it may
/// pass through — so they travel as one argument instead of three. What a request excludes is not
/// here: that changes per solve and travels beside this as a [`ResolvedRouteConstraints`].
#[derive(Debug, Clone)]
pub struct GraphQueryFilter {
    /// Shortest route to return, in hops. A query with `0` matches nothing.
    pub min_hops: usize,
    /// Longest route to return, in hops.
    pub max_hops: usize,
    /// Tokens a route may pass *through*. Its own endpoints and the tokens the request requires
    /// are always allowed, whatever this holds. `None` allows every token.
    pub connector_tokens: Option<FxHashSet<Address>>,
}

/// One solve's route search: the bounds the algorithm configured, and what the request excludes.
///
/// The two have different lifetimes — the bounds are built with the algorithm, the exclusions
/// arrive with the order — so a search borrows both rather than owning either.
#[derive(Debug, Clone, Copy)]
pub struct RouteSearch<'a> {
    /// How long a route may be, and which tokens it may pass through.
    pub bounds: &'a GraphQueryFilter,
    /// Pools and tokens this request excludes.
    pub exclusions: &'a ResolvedRouteConstraints,
}

impl RouteSearch<'_> {
    /// Whether a route may pass through this token: an endpoint always may, and an
    /// intermediate must clear the request's exclusions and either be on the connector list or be
    /// a token the request requires.
    #[must_use]
    pub fn allows_token(self, token: &Address, endpoints: (&Address, &Address)) -> bool {
        self.exclusions
            .allows_token(token, endpoints) &&
            (token == endpoints.0 ||
                token == endpoints.1 ||
                self.bounds
                    .connector_tokens
                    .as_ref()
                    .is_none_or(|tokens| tokens.contains(token)) ||
                self.exclusions
                    .required_tokens()
                    .contains(token))
    }
}

/// Trait for edge weight types that can be computed from a ProtocolSim and DerivedData.
///
/// Implement this trait for edge data types that should use pre-computed derived data
/// (component depths, spot prices, etc.) instead of computing them from scratch.
pub trait EdgeWeightFromSimAndDerived: Sized {
    /// Computes edge weight data using ProtocolSim and pre-computed DerivedData.
    ///
    /// # Arguments
    ///
    /// * `sim` - The protocol simulation state
    /// * `component_id` - The component ID for derived data lookup
    /// * `token_in` - The input token
    /// * `token_out` - The output token
    /// * `derived` - Pre-computed derived data (component depths, spot prices, etc.)
    ///
    /// # Returns
    ///
    /// The computed edge weight, or `None` if it cannot be computed.
    fn from_sim_and_derived(
        sim: &dyn ProtocolSim,
        component_id: &ComponentId,
        token_in: &Token,
        token_out: &Token,
        derived: &DerivedData,
    ) -> Option<Self>;
}

/// Trivial implementation for algorithms that don't use edge weights (e.g., Bellman-Ford).
impl EdgeWeightFromSimAndDerived for () {
    fn from_sim_and_derived(
        _sim: &dyn ProtocolSim,
        _component_id: &ComponentId,
        _token_in: &Token,
        _token_out: &Token,
        _derived: &DerivedData,
    ) -> Option<Self> {
        Some(())
    }
}

/// Trait for graph managers that support edge weight updates with derived data.
pub trait EdgeWeightUpdaterWithDerived {
    /// Updates edge weights using simulation states and pre-computed derived data.
    ///
    /// Returns the number of edges successfully updated.
    fn update_edge_weights_with_derived(
        &mut self,
        market: MarketDataView<'_>,
        derived: &DerivedData,
    ) -> usize;
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{
        algorithm::test_utils::{addr, topology_with_directions},
        feed::events::{MarketEvent, MarketEventHandler},
    };

    /// A graph manager that reports, for a swap direction of a component, whether it has an edge
    /// and whether that edge has a weight.
    trait DirectionGraph: MarketEventHandler {
        fn set_weight(&mut self, component_id: &str, token_in: &Address, token_out: &Address);
        fn weight(
            &self,
            component_id: &str,
            token_in: &Address,
            token_out: &Address,
        ) -> Option<bool>;
    }

    impl DirectionGraph for PetgraphStableDiGraphManager<()> {
        fn set_weight(&mut self, component_id: &str, token_in: &Address, token_out: &Address) {
            self.set_edge_weight(&component_id.to_string(), token_in, token_out, (), false)
                .unwrap();
        }

        fn weight(
            &self,
            component_id: &str,
            token_in: &Address,
            token_out: &Address,
        ) -> Option<bool> {
            let graph = self.graph();
            graph.edge_indices().find_map(|edge| {
                let (from, to) = graph.edge_endpoints(edge)?;
                let data = &graph[edge];
                (data.component_id == component_id &&
                    graph[from] == *token_in &&
                    graph[to] == *token_out)
                    .then(|| data.data.is_some())
            })
        }
    }

    impl DirectionGraph for TopologyGraphManager<()> {
        fn set_weight(&mut self, component_id: &str, token_in: &Address, token_out: &Address) {
            self.set_pool_weight(&component_id.to_string(), token_in, token_out, (), false)
                .unwrap();
        }

        fn weight(
            &self,
            component_id: &str,
            token_in: &Address,
            token_out: &Address,
        ) -> Option<bool> {
            let graph = self.graph();
            let (from, to) = (graph.get_token_ix(token_in)?, graph.get_token_ix(token_out)?);
            graph
                .pools_between(from, to)
                .iter()
                .find(|pool| pool.component_id == component_id)
                .map(|pool| pool.data.is_some())
        }
    }

    #[rstest]
    #[case::petgraph(PetgraphStableDiGraphManager::<()>::default())]
    #[case::topology(TopologyGraphManager::<()>::default())]
    #[tokio::test]
    async fn test_swap_directions_become_edges(#[case] mut manager: impl DirectionGraph) {
        let (a, b, c) = (addr(0x01), addr(0x02), addr(0x03));
        let add_venue = |directions: &[(&Address, &Address)]| MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(
                "venue".to_string(),
                topology_with_directions(&[&a, &b, &c], directions),
            )]),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        };
        let edges = |manager: &dyn DirectionGraph| {
            [(&a, &b), (&b, &a), (&b, &c)].map(|(from, to)| manager.weight("venue", from, to))
        };

        manager
            .handle_event(&add_venue(&[(&a, &b), (&b, &a)]))
            .await
            .unwrap();
        assert_eq!(edges(&manager), [Some(false), Some(false), None]);

        manager.set_weight("venue", &a, &b);
        manager
            .handle_event(&add_venue(&[(&a, &b), (&b, &c)]))
            .await
            .unwrap();
        assert_eq!(edges(&manager), [Some(true), None, Some(false)]);
    }
}
