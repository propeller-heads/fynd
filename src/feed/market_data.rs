//! Shared market data structure.
//!
//! This is the single source of truth for all market data.
//! It's protected by a RwLock and shared across all components:
//! - TychoIndexer: WRITE access to update data
//! - Solvers: READ access to query states during solving
//!
//! We use tokio RwLock (which is write-preferring) to avoid writer starvation.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;
use tycho_simulation::{
    tycho_client::feed::SynchronizerState,
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Address},
        simulation::protocol_sim::ProtocolSim,
    },
};

use crate::types::{BlockInfo, ComponentId, GasPrice};

/// Thread-safe handle to shared market data.
pub type SharedMarketDataRef = Arc<RwLock<SharedMarketData>>;

/// Creates a new shared market data instance wrapped in Arc<RwLock<>>.
pub fn new_shared_market_data() -> SharedMarketDataRef {
    Arc::new(RwLock::new(SharedMarketData::new()))
}

/// Shared market data containing all component states and market information.
///
/// This struct is the single source of truth for market data.
/// The indexer updates it, and solvers read from it.
#[derive(Debug, Default)]
pub struct SharedMarketData {
    /// All components indexed by their ID.
    components: HashMap<ComponentId, ProtocolComponent>,
    /// All states indexed by their component ID.
    simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
    /// All tokens indexed by their address.
    tokens: HashMap<Address, Token>,
    /// Current gas price.
    gas_price: GasPrice,
    /// Protocol sync status indexed by their protocol system name.
    protocol_sync_status: HashMap<String, SynchronizerState>,
    /// Block info for the last update (only updated when protocols reported "Ready" status).
    /// None if no block has been processed yet.
    last_updated: Option<BlockInfo>,
}

impl SharedMarketData {
    /// Creates a new empty SharedMarketData.
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            simulation_states: HashMap::new(),
            tokens: HashMap::new(),
            gas_price: GasPrice::default(),
            protocol_sync_status: HashMap::new(),
            last_updated: None,
        }
    }

    /// Returns the block info for the last update.
    pub fn last_updated(&self) -> Option<&BlockInfo> {
        self.last_updated.as_ref()
    }

    /// Returns the protocol sync status indexed by their protocol system name.
    pub fn get_protocol_sync_status(&self, protocol_system: &String) -> Option<&SynchronizerState> {
        self.protocol_sync_status
            .get(protocol_system)
    }

    /// Returns the component topology.
    /// This is a simple mapping from component ID to their token addresses.
    pub fn component_topology(&self) -> HashMap<ComponentId, Vec<Address>> {
        self.components
            .iter()
            .map(|(id, component)| (id.clone(), component.tokens.clone()))
            .collect()
    }

    /// Gets a component by ID.
    pub fn get_component(&self, id: &str) -> Option<&ProtocolComponent> {
        self.components.get(id)
    }

    /// Gets a simulation state by ID.
    pub fn get_simulation_state(&self, id: &str) -> Option<&dyn ProtocolSim> {
        self.simulation_states
            .get(id)
            .map(|b| b.as_ref())
    }

    /// Gets a token by address.
    pub fn get_token(&self, address: &Address) -> Option<&Token> {
        self.tokens.get(address)
    }

    /// Returns the current gas price.
    pub fn gas_price(&self) -> &GasPrice {
        &self.gas_price
    }

    /// Returns a reference to the component registry.
    pub fn component_registry_ref(&self) -> &HashMap<ComponentId, ProtocolComponent> {
        &self.components
    }

    /// Returns a reference to the simulation state registry.
    pub fn simulation_state_registry_ref(&self) -> &HashMap<ComponentId, Box<dyn ProtocolSim>> {
        &self.simulation_states
    }

    /// Returns a reference to the token registry.
    pub fn token_registry_ref(&self) -> &HashMap<Address, Token> {
        &self.tokens
    }

    /// Inserts or updates a component.
    pub fn upsert_components(&mut self, components: impl IntoIterator<Item = ProtocolComponent>) {
        // Store component data in components map
        for component in components {
            self.components
                .insert(component.id.clone(), component);
        }
    }

    /// Inserts or updates tokens.
    pub fn upsert_tokens(&mut self, tokens: impl IntoIterator<Item = Token>) {
        for token in tokens {
            self.tokens
                .entry(token.address.clone())
                .or_insert_with(|| token);
        }
    }

    /// Updates the protocol sync status.
    pub fn update_protocol_sync_status(
        &mut self,
        sync_states: impl IntoIterator<Item = (String, SynchronizerState)>,
    ) {
        for (protocol_system, status) in sync_states {
            self.protocol_sync_status
                .insert(protocol_system, status);
        }
    }

    /// Removes a component.
    pub fn remove_components<'a>(&mut self, ids: impl IntoIterator<Item = &'a ComponentId>) {
        for id in ids {
            self.components.remove(id);
            self.simulation_states.remove(id);
        }
    }

    /// Updates a component's state.
    pub fn update_states(
        &mut self,
        states: impl IntoIterator<Item = (ComponentId, Box<dyn ProtocolSim>)>,
    ) {
        for (id, state) in states {
            self.simulation_states.insert(id, state);
        }
    }

    /// Updates the gas price.
    pub fn update_gas_price(&mut self, gas_price: GasPrice) {
        self.gas_price = gas_price;
    }

    /// Updates the last updated block info.
    pub fn update_last_updated(&mut self, block_info: BlockInfo) {
        self.last_updated = Some(block_info);
    }
}
