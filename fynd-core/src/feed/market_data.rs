//! Shared market data structure.
//!
//! This is the single source of truth for all market data.
//! It's protected by a RwLock and shared across all components:
//! - TychoIndexer: WRITE access to update data
//! - Solvers: READ access to query states during solving
//!
//! We use tokio RwLock (which is write-preferring) to avoid writer starvation.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tokio::sync::RwLock;
use tycho_simulation::{
    tycho_client::feed::SynchronizerState,
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Address},
        simulation::protocol_sim::ProtocolSim,
    },
    tycho_ethereum::gas::BlockGasPrice,
};

use crate::types::{BlockInfo, ComponentId};

/// Thread-safe handle to shared market data.
pub type SharedMarketDataRef = Arc<RwLock<SharedMarketData>>;

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
    /// Current gas price. None if not fetched yet.
    gas_price: Option<BlockGasPrice>,
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
            gas_price: None,
            protocol_sync_status: HashMap::new(),
            last_updated: None,
        }
    }

    /// Creates a new shared market data store for async computation tests that is wrapped in an
    /// `Arc<RwLock<>>`.
    pub fn new_shared() -> SharedMarketDataRef {
        Arc::new(RwLock::new(Self::new()))
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

    /// Returns the current gas price. None if not fetched yet.
    pub fn gas_price(&self) -> Option<&BlockGasPrice> {
        self.gas_price.as_ref()
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
                .insert(token.address.clone(), token);
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
    pub fn update_gas_price(&mut self, gas_price: BlockGasPrice) {
        self.gas_price = Some(gas_price);
    }

    /// Updates the last updated block info.
    pub fn update_last_updated(&mut self, block_info: BlockInfo) {
        self.last_updated = Some(block_info);
    }

    /// Returns the total number of simulation states.
    pub fn simulation_states_count(&self) -> usize {
        self.simulation_states.len()
    }

    /// Returns pool counts grouped by protocol system.
    pub fn pool_counts_by_protocol(&self) -> HashMap<String, usize> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for component in self.components.values() {
            *counts
                .entry(component.protocol_system.clone())
                .or_default() += 1;
        }
        counts
    }

    /// Creates a filtered subset containing only data needed for the given components.
    ///
    /// This is used to create a local snapshot of market data that can be used for
    /// simulation without holding the main lock. The subset includes:
    /// - Components matching the provided IDs
    /// - Simulation states for those components (cloned via `clone_box`)
    /// - Tokens referenced by those components
    /// - Gas price and block info
    pub fn extract_subset(&self, component_ids: &HashSet<ComponentId>) -> SharedMarketData {
        // Filter components
        let components: HashMap<ComponentId, ProtocolComponent> = self
            .components
            .iter()
            .filter(|(id, _)| component_ids.contains(*id))
            .map(|(id, component)| (id.clone(), component.clone()))
            .collect();

        // Collect all token addresses from the filtered components
        let token_addresses: HashSet<&Address> = components
            .values()
            .flat_map(|c| &c.tokens)
            .collect();

        // Filter tokens
        let tokens: HashMap<Address, Token> = self
            .tokens
            .iter()
            .filter(|(addr, _)| token_addresses.contains(addr))
            .map(|(addr, token)| (addr.clone(), token.clone()))
            .collect();

        // Clone simulation states using clone_box
        let simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>> = self
            .simulation_states
            .iter()
            .filter(|(id, _)| component_ids.contains(*id))
            .map(|(id, state)| (id.clone(), state.clone_box()))
            .collect();

        SharedMarketData {
            components,
            simulation_states,
            tokens,
            gas_price: self.gas_price.clone(),
            protocol_sync_status: HashMap::new(), // Not needed for simulation
            last_updated: self.last_updated.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::tycho_ethereum::gas::GasPrice;

    use super::*;
    use crate::algorithm::test_utils::{component, token, MockProtocolSim};

    #[test]
    fn extract_subset_filters_by_component_ids() {
        // Setup: market with 2 pools (A-B, B-C) and 3 tokens
        let mut market = SharedMarketData::new();

        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        market.upsert_components([
            component("pool_ab", &[token_a.clone(), token_b.clone()]),
            component("pool_bc", &[token_b.clone(), token_c.clone()]),
        ]);
        market.upsert_tokens([token_a.clone(), token_b.clone(), token_c.clone()]);
        market.update_states([
            ("pool_ab".to_string(), Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>),
            ("pool_bc".to_string(), Box::new(MockProtocolSim::new(3.0)) as Box<dyn ProtocolSim>),
        ]);
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });
        market.update_last_updated(BlockInfo::new(12345, "0xabc".to_string(), 0));

        // Extract only pool_ab
        let ids: HashSet<_> = ["pool_ab".to_string()]
            .into_iter()
            .collect();
        let subset = market.extract_subset(&ids);

        // Components: only pool_ab
        assert_eq!(subset.components.len(), 1);
        assert!(subset
            .components
            .contains_key("pool_ab"));

        // Tokens: only A and B (referenced by pool_ab), not C
        assert_eq!(subset.tokens.len(), 2);
        assert!(subset
            .tokens
            .contains_key(&token_a.address));
        assert!(subset
            .tokens
            .contains_key(&token_b.address));
        assert!(!subset
            .tokens
            .contains_key(&token_c.address));

        // Simulation states: only pool_ab
        assert_eq!(subset.simulation_states.len(), 1);
        assert!(subset
            .simulation_states
            .contains_key("pool_ab"));

        // Gas price and block info are copied
        assert_eq!(subset.gas_price, market.gas_price);
        assert!(subset.last_updated.is_some());

        // Empty IDs returns empty subset
        let empty_subset = market.extract_subset(&HashSet::new());
        assert!(empty_subset.components.is_empty());
        assert!(empty_subset.tokens.is_empty());
        assert!(empty_subset
            .simulation_states
            .is_empty());
    }
}
