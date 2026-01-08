//! Petgraph implementation of GraphManager.
//!
//! This module provides PetgraphGraphManager, which implements GraphManager
//! for petgraph::graph::UnGraph, providing a reusable implementation for
//! algorithms that use petgraph.

use crate::events::MarketEvent;
use crate::types::PoolId;
use petgraph::Graph;
use std::collections::HashMap;
use tycho_common::models::Address;

use super::{Edge, GraphManager};

/// Petgraph implementation of GraphManager.
///
/// This struct implements GraphManager for petgraph::graph::UnGraph, providing
/// a reusable implementation for algorithms that use petgraph.
///
/// The graph manager maintains the graph internally and updates it based on market events.
pub struct PetgraphGraphManager {
    graph: petgraph::graph::UnGraph<Address, Edge>,
}

impl PetgraphGraphManager {
    /// Creates a new PetgraphGraphManager with an empty graph.
    pub fn new() -> Self {
        Self {
            graph: Graph::new_undirected(),
        }
    }
}

impl Default for PetgraphGraphManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(unused_variables)] // TODO: Implement these methods and remove this allow
impl GraphManager<petgraph::graph::UnGraph<Address, Edge>> for PetgraphGraphManager {
    fn initialize_graph(&mut self, pools: &HashMap<PoolId, Vec<Address>>) {
        unimplemented!("initialize_graph is not implemented for PetgraphGraphManager");
    }

    fn graph(&self) -> &petgraph::graph::UnGraph<Address, Edge> {
        &self.graph
    }

    fn handle_event(&mut self, event: &MarketEvent) {
        unimplemented!("handle_event is not implemented for PetgraphGraphManager");
    }
}
