//! Market events for communication between the indexer and solvers.
//!
//! The indexer broadcasts these events when market data changes.
//! Solvers subscribe to these events to keep their local graph in sync.

use tycho_common::models::Address;

use crate::types::{GasPrice, PoolId, ProtocolSystem};

/// Events broadcast by the indexer when market data changes.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// A new pool was added to the market.
    PoolAdded {
        pool_id: PoolId,
        tokens: Vec<Address>,
        protocol_system: ProtocolSystem,
    },

    /// A pool was removed from the market.
    PoolRemoved { pool_id: PoolId },

    /// A pool's state was updated (reserves changed, etc.).
    /// Solvers should re-read the state from SharedMarketData if needed.
    StateUpdated { pool_id: PoolId },

    /// Gas price was updated.
    GasPriceUpdated { gas_price: GasPrice },

    /// Full market snapshot.
    /// Sent to new subscribers for initial synchronization.
    Snapshot {
        pools: Vec<PoolSummary>,
        gas_price: GasPrice,
    },
}

/// Summary of a pool for snapshot events.
#[derive(Debug, Clone)]
pub struct PoolSummary {
    pub id: PoolId,
    pub tokens: Vec<Address>,
    pub protocol_system: ProtocolSystem,
}

impl PoolSummary {
    pub fn new(id: PoolId, tokens: Vec<Address>, protocol_system: ProtocolSystem) -> Self {
        Self {
            id,
            tokens,
            protocol_system,
        }
    }
}

/// Trait for components that can receive market events.
pub trait MarketEventHandler {
    /// Handle a market event.
    fn handle_event(&mut self, event: &MarketEvent);
}
