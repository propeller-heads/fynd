//! Shared market data structure.
//!
//! This is the single source of truth for all market data.
//! It's protected by a RwLock and shared across all components:
//! - TychoIndexer: WRITE access to update data
//! - Solvers: READ access to query states during solving

use std::collections::HashMap;
use std::sync::Arc;
use tycho_common::dto::Block;

use crate::types::{GasPrice, PoolId, ProtocolSystem, Token};
use alloy::primitives::Address;
use tokio::sync::RwLock;

/// Thread-safe handle to shared market data.
pub type SharedMarketDataRef = Arc<RwLock<SharedMarketData>>;

/// Creates a new shared market data instance wrapped in Arc<RwLock<>>.
pub fn new_shared_market_data() -> SharedMarketDataRef {
    Arc::new(RwLock::new(SharedMarketData::new()))
}

/// Shared market data containing all pool states and market information.
///
/// This struct is the single source of truth for market data.
/// The indexer updates it, and solvers read from it.
pub struct SharedMarketData {
    /// All pools indexed by their ID.
    pools: HashMap<PoolId, PoolData>,
    /// All tokens indexed by their address.
    tokens: HashMap<Address, Token>,
    /// Market topology: pool_id -> tokens in that pool.
    /// This is the source of truth for graph construction.
    pool_topology: HashMap<PoolId, Vec<Address>>,
    /// Current gas price.
    gas_price: GasPrice,
    /// Gas costs per protocol system.
    gas_constants: HashMap<ProtocolSystem, u64>,
    /// When the data was last updated.
    last_updated: Block,
}

/// Data for a single pool.
pub struct PoolData {
    /// Unique identifier.
    pub id: PoolId,
    /// Protocol component (from Tycho).
    /// TODO: Replace with actual ProtocolComponent type from tycho-simulation
    pub component: ProtocolComponent,
    /// Protocol simulation state (from Tycho).
    /// TODO: Replace with actual Box<dyn ProtocolSim> from tycho-simulation
    pub state: ProtocolState,
    /// Tokens in this pool.
    pub tokens: Vec<Token>,
    /// Protocol system for gas estimation.
    pub protocol_system: ProtocolSystem,
}

/// Placeholder for Tycho's ProtocolComponent.
/// TODO: Replace with actual type from tycho-simulation crate.
#[derive(Debug, Clone)]
pub struct ProtocolComponent {
    pub id: String,
    pub protocol: String,
    pub tokens: Vec<Address>,
}

/// Placeholder for Tycho's ProtocolSim state.
/// TODO: Replace with actual Box<dyn ProtocolSim> from tycho-simulation crate.
pub struct ProtocolState {
    // Placeholder - will contain actual simulation state
    _data: Vec<u8>,
}

impl ProtocolState {
    pub fn new() -> Self {
        Self { _data: vec![] }
    }
}

impl Default for ProtocolState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedMarketData {
    /// Creates a new empty SharedMarketData.
    pub fn new() -> Self {
        let mut gas_constants = HashMap::new();
        // Initialize default gas costs
        gas_constants.insert(ProtocolSystem::UniswapV2, 100_000);
        gas_constants.insert(ProtocolSystem::UniswapV3, 150_000);
        gas_constants.insert(ProtocolSystem::SushiSwap, 100_000);
        gas_constants.insert(ProtocolSystem::Curve, 200_000);
        gas_constants.insert(ProtocolSystem::Balancer, 150_000);
        gas_constants.insert(ProtocolSystem::Other, 150_000);

        Self {
            pools: HashMap::new(),
            tokens: HashMap::new(),
            pool_topology: HashMap::new(),
            gas_price: GasPrice::default(),
            gas_constants,
            last_updated: Block::default(),
        }
    }

    // ==================== Read Methods (for Solvers) ====================

    /// Gets a pool by ID.
    pub fn get_pool(&self, id: &PoolId) -> Option<&PoolData> {
        self.pools.get(id)
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

    /// Returns a clone of the pool topology.
    ///
    /// Solvers can use this to build their algorithm-specific graphs.
    pub fn pool_topology(&self) -> HashMap<PoolId, Vec<Address>> {
        self.pool_topology.clone()
    }

    /// Returns a reference to the pool topology.
    pub fn pool_topology_ref(&self) -> &HashMap<PoolId, Vec<Address>> {
        &self.pool_topology
    }

    /// Returns the number of pools.
    pub fn pool_count(&self) -> usize {
        self.pools.len()
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
        self.last_updated.ts.and_utc().timestamp() as u64
    }

    /// Returns an iterator over all pools.
    pub fn pools(&self) -> impl Iterator<Item = (&PoolId, &PoolData)> {
        self.pools.iter()
    }

    // ==================== Write Methods (for Indexer only) ====================

    /// Adds a pool to the topology without full pool data.
    /// Used when we receive pool info from Tycho but don't have full state yet.
    pub fn add_pool_topology(
        &mut self,
        pool_id: PoolId,
        tokens: Vec<Address>,
    ) {
        self.pool_topology.insert(pool_id, tokens);
        self.last_updated = Block::default();
    }

    /// Inserts or updates a pool.
    pub fn insert_pool(&mut self, pool: PoolData) {
        let pool_id = pool.id.clone();
        let tokens: Vec<Address> = pool.tokens.iter().map(|t| t.address).collect();

        // Update tokens map
        for token in &pool.tokens {
            self.tokens
                .entry(token.address)
                .or_insert_with(|| token.clone());
        }

        // Update pool topology
        self.pool_topology.insert(pool_id.clone(), tokens);

        // Store pool data
        self.pools.insert(pool_id, pool);

        self.last_updated = Block::default();
    }

    /// Removes a pool.
    pub fn remove_pool(&mut self, id: &PoolId) {
        if self.pools.remove(id).is_some() {
            self.pool_topology.remove(id);
            self.last_updated = Block::default();
        }
    }

    /// Updates a pool's state.
    pub fn update_state(&mut self, id: &PoolId, state: ProtocolState) {
        if let Some(pool) = self.pools.get_mut(id) {
            pool.state = state;
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

impl Default for SharedMarketData {
    fn default() -> Self {
        Self::new()
    }
}
