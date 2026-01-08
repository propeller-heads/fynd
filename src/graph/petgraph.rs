//! Petgraph's UnGraph implementation of GraphManager.
//!
//! This module provides PetgraphUnGraphManager, which implements GraphManager for
//! petgraph::graph::UnGraph, providing a reusable implementation for algorithms that use petgraph.

use std::collections::HashMap;

use petgraph::Graph;
use tycho_common::models::Address;

use super::GraphManager;
use crate::{feed::events::MarketEvent, types::ComponentId};

/// Petgraph implementation of GraphManager.
///
/// This struct implements GraphManager for petgraph::graph::UnGraph, providing
/// a reusable implementation for algorithms that use petgraph.
///
/// The graph manager maintains the graph internally and updates it based on market events.
pub struct PetgraphUnGraphManager {
    // Undirected graph with token addresses as nodes and edges as possible swaps.
    graph: petgraph::graph::UnGraph<Address, ComponentId>,
}

impl PetgraphUnGraphManager {
    /// Creates a new PetgraphUnGraphManager with an empty graph.
    pub fn new() -> Self {
        Self { graph: Graph::new_undirected() }
    }
}

impl Default for PetgraphUnGraphManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(unused_variables)]
impl GraphManager<petgraph::graph::UnGraph<Address, ComponentId>> for PetgraphUnGraphManager {
    fn initialize_graph(&mut self, components: &HashMap<ComponentId, Vec<Address>>) {
        unimplemented!("initialize_graph is not implemented for PetgraphGraphManager");
    }

    fn graph(&self) -> &petgraph::graph::UnGraph<Address, ComponentId> {
        &self.graph
    }

    fn handle_event(&mut self, event: &MarketEvent) {
        unimplemented!("handle_event is not implemented for PetgraphUnGraphManager");
    }
}
