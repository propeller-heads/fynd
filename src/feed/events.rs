//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their local graph in sync.

use std::collections::HashMap;

use tycho_simulation::tycho_core::models::Address;

use crate::types::{ComponentId, GasPrice};

/// Events broadcast by the indexer when market data changes.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// Market was updated.
    MarketUpdated {
        added_components: HashMap<ComponentId, Vec<Address>>,
        removed_components: Vec<ComponentId>,
        updated_components: Vec<ComponentId>,
    },

    /// Gas price was updated.
    GasPriceUpdated { gas_price: GasPrice },
}

/// Trait for components that can receive market events.
pub trait MarketEventHandler {
    /// Handle a market event.
    fn handle_event(&mut self, event: &MarketEvent);
}
