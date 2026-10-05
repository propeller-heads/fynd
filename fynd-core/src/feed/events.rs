//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their local graph in sync.

use async_trait::async_trait;
use rustc_hash::FxHashMap;
use thiserror::Error;
use tokio::sync::broadcast;
use tycho_simulation::tycho_common::models::Address;

use crate::{graph::GraphError, types::ComponentId};

/// Events broadcast by the indexer when market data changes.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum MarketEvent {
    /// Market was updated.
    MarketUpdated {
        /// Components added in this update, keyed by component ID.
        added_components: FxHashMap<ComponentId, Vec<Address>>,
        /// Component IDs that were removed.
        removed_components: Vec<ComponentId>,
        /// Component IDs whose state changed.
        updated_components: Vec<ComponentId>,
    },
}

/// A handle that can subscribe to [`MarketEvent`]s but cannot send them. Cloning is cheap.
#[derive(Clone)]
pub struct MarketEvents(broadcast::Sender<MarketEvent>);

impl MarketEvents {
    /// Returns a handle that subscribes to the events that `sender` broadcasts.
    pub fn new(sender: broadcast::Sender<MarketEvent>) -> Self {
        Self(sender)
    }

    /// Returns a new receiver of the events sent after this call.
    ///
    /// A receiver that falls behind the channel capacity loses the oldest events and gets
    /// `RecvError::Lagged`.
    pub fn subscribe(&self) -> broadcast::Receiver<MarketEvent> {
        self.0.subscribe()
    }
}

/// Errors that can occur when handling market events.
#[derive(Error, Debug)]
pub enum EventError {
    /// Graph-related errors
    #[error("graph errors: {0:?}")]
    GraphErrors(Vec<GraphError>),
}

/// Trait for components that can receive market events.
#[async_trait]
pub trait MarketEventHandler: Send {
    /// Handle a market event.
    ///
    /// # Errors
    ///
    /// Returns an error if the event could not be processed.
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError>;
}
