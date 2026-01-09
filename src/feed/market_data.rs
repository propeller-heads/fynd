//! Shared market data structure.
//!
//! This is the single source of truth for all market data.
//! It's protected by a RwLock and shared across all components:
//! - TychoIndexer: WRITE access to update data
//! - Solvers: READ access to query states during solving

use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;
use tycho_simulation::tycho_core::{
    dto::Block,
    models::{protocol::ProtocolComponent, token::Token, Address},
    simulation::protocol_sim::ProtocolSim,
};

use crate::types::{constants::GAS_COST_PER_SWAP, ComponentId, GasPrice, ProtocolSystem};

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
    components: HashMap<ComponentId, ComponentData>,
    /// All tokens indexed by their address.
    tokens: HashMap<Address, Token>,
    /// Market topology: component_id -> tokens in that component.
    /// This is the source of truth for graph construction.
    component_topology: HashMap<ComponentId, Vec<Address>>,
    /// Current gas price.
    gas_price: GasPrice,
    /// Average gas cost per swap on protocol system.
    gas_constants: HashMap<ProtocolSystem, u64>,
    /// When the data was last updated.
    last_updated: Block,
}

/// Data for a single component.
#[derive(Debug)]
pub struct ComponentData {
    /// Protocol component information.
    pub component: ProtocolComponent,
    /// Protocol simulation object.
    pub state: Box<dyn ProtocolSim>,
    /// Tokens in this component.
    pub tokens: Vec<Token>,
}

impl SharedMarketData {
    /// Creates a new empty SharedMarketData.
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
            tokens: HashMap::new(),
            component_topology: HashMap::new(),
            gas_price: GasPrice::default(),
            gas_constants: GAS_COST_PER_SWAP.clone(),
            last_updated: Block::default(),
        }
    }

    // ==================== Read Methods (for Solvers) ====================

    /// Gets a component by ID.
    pub fn get_component(&self, id: &ComponentId) -> Option<&ComponentData> {
        self.components.get(id)
    }

    /// Gets a token by address.
    pub fn get_token(&self, address: &Address) -> Option<&Token> {
        self.tokens.get(address)
    }

    /// Returns the current gas price.
    pub fn gas_price(&self) -> &GasPrice {
        &self.gas_price
    }

    /// Returns the gas cost for a protocol system.
    pub fn gas_cost(&self, protocol: ProtocolSystem) -> u64 {
        self.gas_constants
            .get(&protocol)
            .copied()
            .unwrap_or(150_000)
    }

    /// Returns a clone of the component topology.
    ///
    /// Solvers can use this to build their algorithm-specific graphs.
    pub fn component_topology(&self) -> HashMap<ComponentId, Vec<Address>> {
        self.component_topology.clone()
    }

    /// Returns a reference to the component topology.
    pub fn component_topology_ref(&self) -> &HashMap<ComponentId, Vec<Address>> {
        &self.component_topology
    }

    /// Returns the number of components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Returns the number of tokens.
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Returns when the data was last updated.
    pub fn last_updated(&self) -> Block {
        self.last_updated.clone()
    }

    /// Returns the age of the data in milliseconds.
    pub fn age_ms(&self) -> u64 {
        self.last_updated
            .ts
            .and_utc()
            .timestamp() as u64
    }

    /// Returns an iterator over all components.
    pub fn components(&self) -> impl Iterator<Item = (&ComponentId, &ComponentData)> {
        self.components.iter()
    }

    // ==================== Write Methods (for Indexer only) ====================

    /// Adds a component to the topology without full component data.
    /// Used when we receive component info from Tycho but don't have full state yet.
    pub fn add_component_topology(&mut self, component_id: ComponentId, tokens: Vec<Address>) {
        self.component_topology
            .insert(component_id, tokens);
        self.last_updated = Block::default();
    }

    /// Inserts or updates a component.
    pub fn insert_component(&mut self, component_data: ComponentData) {
        let component_id = component_data.component.id.clone();
        let tokens: Vec<Address> = component_data
            .tokens
            .iter()
            .map(|t| t.address.clone())
            .collect();

        // Update tokens map
        for token in &component_data.tokens {
            self.tokens
                .entry(token.address.clone())
                .or_insert_with(|| token.clone());
        }

        // Update component topology
        self.component_topology
            .insert(component_id.clone(), tokens);

        // Store component data
        self.components
            .insert(component_id, component_data);

        self.last_updated = Block::default();
    }

    /// Removes a component.
    pub fn remove_component(&mut self, id: &ComponentId) {
        if self.components.remove(id).is_some() {
            self.component_topology.remove(id);
            self.last_updated = Block::default();
        }
    }

    /// Updates a component's state.
    pub fn update_state(&mut self, id: &ComponentId, state: Box<dyn ProtocolSim>) {
        if let Some(component_data) = self.components.get_mut(id) {
            component_data.state = state;
            self.last_updated = Block::default();
        }
    }

    /// Updates the gas price.
    pub fn update_gas_price(&mut self, gas_price: GasPrice) {
        self.gas_price = gas_price;
        self.last_updated = Block::default();
    }

    /// Updates gas constants for a protocol.
    pub fn set_gas_constant(&mut self, protocol: ProtocolSystem, gas: u64) {
        self.gas_constants.insert(protocol, gas);
    }
}
