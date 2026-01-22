//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their local graph in sync.

use std::collections::HashMap;

use thiserror::Error;
use tycho_simulation::tycho_common::models::Address;

use crate::{graph::GraphError, types::ComponentId};

/// Events broadcast by the indexer when market data changes.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum MarketEvent {
    /// Market was updated.
    MarketUpdated {
        added_components: HashMap<ComponentId, Vec<Address>>,
        removed_components: Vec<ComponentId>,
        updated_components: Vec<ComponentId>,
    },
}

/// Errors that can occur when handling market events.
#[derive(Error, Debug)]
pub enum EventError {
    /// Graph-related errors
    #[error("graph errors: {0:?}")]
    GraphErrors(Vec<GraphError>),
    /// Invalid event data.
    #[error("invalid event: {0}")]
    InvalidEvent(String),
}

/// Trait for components that can receive market events.
pub trait MarketEventHandler {
    /// Handle a market event.
    ///
    /// # Errors
    ///
    /// Returns an error if the event could not be processed.
    fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError>;
}
