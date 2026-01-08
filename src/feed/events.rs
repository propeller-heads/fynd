//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their local graph in sync.

use tycho_common::models::Address;

use crate::types::{ComponentId, GasPrice, ProtocolSystem};

/// Events broadcast by the indexer when market data changes.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// A new component was added to the market.
    ComponentAdded {
        component_id: ComponentId,
        tokens: Vec<Address>,
        protocol_system: ProtocolSystem,
    },

    /// A component was removed from the market.
    ComponentRemoved { component_id: ComponentId },

    /// A component's state was updated (reserves changed, etc.).
    /// Solvers should re-read the state from SharedMarketData if needed.
    StateUpdated { component_id: ComponentId },

    /// Gas price was updated.
    GasPriceUpdated { gas_price: GasPrice },

    /// Full market snapshot.
    /// Sent to new subscribers for initial synchronization.
    Snapshot { components: Vec<ComponentSummary>, gas_price: GasPrice },
}

/// Summary of a component for snapshot events.
#[derive(Debug, Clone)]
pub struct ComponentSummary {
    pub id: ComponentId,
    pub tokens: Vec<Address>,
    pub protocol_system: ProtocolSystem,
}

impl ComponentSummary {
    pub fn new(id: ComponentId, tokens: Vec<Address>, protocol_system: ProtocolSystem) -> Self {
        Self { id, tokens, protocol_system }
    }
}

/// Trait for components that can receive market events.
pub trait MarketEventHandler {
    /// Handle a market event.
    fn handle_event(&mut self, event: &MarketEvent);
}
