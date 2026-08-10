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
    tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim},
    tycho_core::models::token::Token,
};

use crate::{derived::DerivedData, feed::market_data::MarketDataView, types::ComponentId};

/// Tokens held without allocating. A path of `h` hops names `h + 1` tokens, so this covers every
/// `max_hops` up to 4. A deeper path still works: `SmallVec` moves to the heap and behaves as a
/// `Vec` from there.
pub(crate) const INLINE_TOKENS: usize = 5;

/// Edges held without allocating. A path's edges are its tokens less one, and an edge is a leg of
/// a route, so this sizes per-leg buffers too.
pub(crate) const INLINE_EDGES: usize = INLINE_TOKENS - 1;

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

    /// Creates a new reversed Path from the current one.
    pub fn reversed(self) -> Self {
        let reversed_tokens = self.tokens.into_iter().rev().collect();
        let reversed_edge_data = self
            .edge_data
            .into_iter()
            .rev()
            .collect();
        Self { tokens: reversed_tokens, edge_data: reversed_edge_data }
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
    #[cfg(test)]
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
    /// - components: A map of component IDs to their tokens addresses.
    fn initialize_graph(&mut self, components: &FxHashMap<ComponentId, Vec<Address>>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;
}

/// What a caller will accept from a route search.
///
/// Bundles the bounds every path query carries so they travel as one argument instead of three.
pub struct GraphQueryFilter {
    /// Shortest route to return, in hops. A query with `0` matches nothing.
    pub min_hops: usize,
    /// Longest route to return, in hops.
    pub max_hops: usize,
    /// Tokens a route may pass *through*. Its own endpoints are always allowed, whatever this
    /// holds. `None` allows every token.
    pub connector_tokens: Option<FxHashSet<Address>>,
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
