//! Graph management for algorithms.
//!
//! This module provides the GraphManager trait which solvers use to manage their market graph
//! representation. GraphManager handles both building graphs from market data and updating them
//! based on market events.

pub mod petgraph;

use std::collections::HashMap;

pub use petgraph::{EdgeData, EdgeIndex, PetgraphStableDiGraphManager};
use thiserror::Error;
use tycho_simulation::tycho_common::models::Address;

use crate::types::ComponentId;

/// A path through the graph as a sequence of edge indices.
///
/// Each edge index points to an edge in the graph containing the component ID and weight.
/// This representation allows O(1) access to edge data during scoring and simulation.
pub type Path = Vec<EdgeIndex>;

#[derive(Error, Debug)]
pub enum GraphError {
    #[error("Token not found in graph: {0:?}")]
    TokenNotFound(Address),
    #[error("Components not found in graph: {0:?}")]
    ComponentsNotFound(Vec<ComponentId>),
    #[error("Components with less then 2 tokens cannot be added: {0:?}")]
    InvalidComponents(Vec<ComponentId>),
    #[error("No edge found between tokens {0:?} and {1:?} for component {2}")]
    MissingComponentBetweenTokens(Address, Address, ComponentId),
    #[error("Invalid hop range: min_hops ({0}) > max_hops ({1})")]
    InvalidHopRange(usize, usize),
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
    fn initialize_graph(&mut self, components: &HashMap<ComponentId, Vec<Address>>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;
}
