//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their internal state in sync.

use std::collections::HashMap;

use tycho_simulation::tycho_common::models::Address;

use crate::types::{ComponentId, GasPrice};

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

    /// Gas price was updated.
    GasPriceUpdated { gas_price: GasPrice },
}
