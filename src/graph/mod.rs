//! Graph management for algorithms.
//!
//! This module provides the GraphManager trait which algorithms implement
//! to manage their graph representation. GraphManager handles both building
//! graphs from market data and updating them based on market events.

use crate::events::MarketEvent;
use crate::types::{PoolId, ProtocolSystem};
use alloy::primitives::Address;
use std::collections::HashMap;

/// An edge in the market graph representing a possible swap.
///
/// This is used as the edge weight type in petgraph graphs.
/// It stores the pool information needed for route construction.
#[derive(Debug, Clone)]
pub struct Edge {
    /// The pool that enables this swap.
    pub pool_id: PoolId,
    /// The output token of this swap.
    pub token_out: Address,
    /// The protocol system (for gas estimation).
    pub protocol_system: ProtocolSystem,
}

/// A path through the graph (sequence of edges).
///
/// This is a shared type that can be used by multiple algorithms
/// to represent a sequence of swaps.
#[derive(Debug, Clone)]
pub struct Path {
    /// The edges in this path, in order.
    pub edges: Vec<Edge>,
    /// The tokens in this path, including start and end.
    pub tokens: Vec<Address>,
}

impl Path {
    /// Returns the number of hops (swaps) in this path.
    pub fn hop_count(&self) -> usize {
        self.edges.len()
    }

    /// Returns the starting token.
    pub fn start_token(&self) -> Option<Address> {
        self.tokens.first().copied()
    }

    /// Returns the ending token.
    pub fn end_token(&self) -> Option<Address> {
        self.tokens.last().copied()
    }
}

/// Trait for managing algorithm-specific graph representations.
///
/// Graph managers are stateful - they maintain the graph internally and update it
/// based on market events. The solver initializes the graph on startup using topology
/// from SharedMarketData, and then the graph manager maintains it.
pub trait GraphManager<G>: Send + Sync
where
    G: Send + Sync,
{
    /// Initializes the graph from the market topology.
    ///
    /// This is called once on solver startup to build the initial graph.
    /// The `pools` parameter maps pool IDs to the tokens they contain.
    fn initialize_graph(&mut self, pools: &HashMap<PoolId, Vec<Address>>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;

    /// Updates the graph based on a market event.
    ///
    /// This method is called by the solver when market events occur.
    fn handle_event(&mut self, event: &MarketEvent);
}

pub mod petgraph;

pub use petgraph::PetgraphGraphManager;
