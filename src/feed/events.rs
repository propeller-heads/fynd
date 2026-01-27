//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their local graph in sync.

use std::collections::HashMap;

use async_trait::async_trait;
use thiserror::Error;
use tycho_simulation::tycho_common::models::Address;

use crate::{graph::GraphError, types::ComponentId};

/// Events broadcast by the indexer when market data changes.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum MarketEvent {
    /// Market was updated.
    MarketUpdated {
        added_components: HashMap<ComponentId, Vec<Address>>,
        removed_components: Vec<ComponentId>,
        #[allow(dead_code)]
        updated_components: Vec<ComponentId>,
    },
}

/// Errors that can occur when handling market events.
#[derive(Error, Debug)]
pub(crate) enum EventError {
    /// Graph-related errors
    #[error("graph errors: {0:?}")]
    GraphErrors(Vec<GraphError>),
    /// Invalid event data.
    #[error("invalid event: {0}")]
    #[allow(dead_code)]
    InvalidEvent(String),
}

/// Trait for components that can receive market events.
#[async_trait]
pub(crate) trait MarketEventHandler: Send {
    /// Handle a market event.
    ///
    /// # Errors
    ///
    /// Returns an error if the event could not be processed.
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError>;
}
