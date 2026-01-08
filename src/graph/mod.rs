//! Graph management for algorithms.
//!
//! This module provides the GraphManager trait which solvers use to manage their market graph
//! representation. GraphManager handles both building graphs from market data and updating them
//! based on market events.

pub mod petgraph;

use std::collections::HashMap;

pub use petgraph::PetgraphUnGraphManager;
use tycho_common::models::Address;

use crate::{events::MarketEvent, types::PoolId};

/// An edge in the market graph representing a possible hop.
///
/// This is used as the edge weight type in petgraph graphs. It stores the pool information needed
/// for route construction.
#[derive(Debug, Clone)]
pub struct Hop {
    /// The pool that enables this swap.
    pub pool_id: PoolId,
    /// The output token of this swap.
    pub token_out: Address,
}

/// A path through the graph (sequence of hops).
///
/// This is a shared type that can be used to represent a sequence of swaps.
#[derive(Debug, Clone)]
pub struct Path {
    /// The hops in this path, in order.
    pub hops: Vec<Hop>,
    /// The tokens in this path, including start and end.
    pub tokens: Vec<Address>,
}

impl Path {
    /// Returns the number of hops (swaps) in this path.
    pub fn hop_count(&self) -> usize {
        self.hops.len()
    }

    /// Returns the starting token.
    pub fn start_token(&self) -> Option<Address> {
        self.tokens.first().cloned()
    }

    /// Returns the ending token.
    pub fn end_token(&self) -> Option<Address> {
        self.tokens.last().cloned()
    }
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
    /// The `pools` parameter maps pool IDs to the tokens they contain.
    fn initialize_graph(&mut self, pools: &HashMap<PoolId, Vec<Address>>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;

    /// Updates the graph based on a market event.
    ///
    /// This method is called by the solver when market events occur.
    fn handle_event(&mut self, event: &MarketEvent);
}
