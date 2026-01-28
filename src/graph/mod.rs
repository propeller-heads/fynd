//! Graph management for algorithms.
//!
//! This module provides the GraphManager trait which solvers use to manage their market graph
//! representation. GraphManager handles both building graphs from market data and updating them
//! based on market events.

pub mod petgraph;

use std::collections::HashMap;

pub use petgraph::{EdgeData, EdgeIndex, PetgraphStableDiGraphManager};
use thiserror::Error;
use tycho_simulation::{
    tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim},
    tycho_core::models::token::Token,
};

use crate::types::ComponentId;

/// A path through the graph as a sequence of edge indices.
///
/// Each edge index points to an edge in the graph containing the component ID and weight.
/// This representation allows O(1) access to edge data during scoring and simulation.
#[derive(Clone, Default)]
pub(crate) struct Path<'a, D> {
    /// Sequence of token addresses in the path.
    pub tokens: Vec<&'a Address>,
    /// Sequence of edge indices representing the path. Length is tokens.len() - 1.
    pub edge_data: Vec<&'a EdgeData<D>>,
}

impl<'a, D> Path<'a, D> {
    /// Creates a new empty Path.
    pub fn new() -> Self {
        Self { tokens: Vec::new(), edge_data: Vec::new() }
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

#[derive(Error, Debug)]
pub(crate) enum GraphError {
    #[error("Token not found in graph: {0:?}")]
    TokenNotFound(Address),
    #[error("Components not found in graph: {0:?}")]
    ComponentsNotFound(Vec<ComponentId>),
    #[error("Components with less then 2 tokens cannot be added: {0:?}")]
    InvalidComponents(Vec<ComponentId>),
    #[error("No edge found between tokens {0:?} and {1:?} for component {2}")]
    MissingComponentBetweenTokens(Address, Address, ComponentId),
}

/// Trait for managing graph representations.
///
/// Graph managers are stateful - they maintain the graph internally and update it based on market
/// events.
pub(crate) trait GraphManager<G>: Send + Sync
where
    G: Send + Sync,
{
    /// Initializes the graph from the market topology.
    ///
    /// Arguments:
    /// - components: A map of component IDs to their tokens addresses.
    fn initialize_graph(&mut self, components: &HashMap<ComponentId, Vec<Address>>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;
}

use crate::{derived::PoolDepths, feed::market_data::SharedMarketData};

/// Trait for edge weight types that can be computed from a ProtocolSim and derived PoolDepths.
///
/// Implement this trait for edge data types that should use pre-computed pool depths
/// from derived data instead of computing them from scratch.
pub trait EdgeWeightFromSimAndDepths: Sized {
    /// Computes edge weight data using spot price from ProtocolSim and depth from PoolDepths.
    ///
    /// # Arguments
    ///
    /// * `sim` - The protocol simulation state (used for spot price)
    /// * `component_id` - The component ID for pool depth lookup
    /// * `token_in` - The input token
    /// * `token_out` - The output token
    /// * `pool_depths` - Pre-computed pool depths from derived data
    ///
    /// # Returns
    ///
    /// The computed edge weight, or `None` if it cannot be computed.
    fn from_sim_and_depths(
        sim: &dyn ProtocolSim,
        component_id: &ComponentId,
        token_in: &Token,
        token_out: &Token,
        pool_depths: &PoolDepths,
    ) -> Option<Self>;
}

/// Trait for graph managers that support edge weight updates with derived data.
pub trait EdgeWeightUpdaterWithDepths {
    /// Updates edge weights using simulation states and pre-computed pool depths.
    ///
    /// Returns the number of edges successfully updated.
    fn update_edge_weights_with_depths(
        &mut self,
        market: &SharedMarketData,
        pool_depths: &PoolDepths,
    ) -> usize;
}
