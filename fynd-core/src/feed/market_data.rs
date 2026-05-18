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
    fmt,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
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

/// Identifies a named simulation-state overlay within [`SharedMarketData`].
///
/// Label format is convention-based: `"ephemeral:<seq>"`, `"block:<number>"`, etc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateLabel(String);

impl StateLabel {
    /// Creates a new `StateLabel` from any string-like value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the label as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StateLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Thread-safe handle to shared market data.
///
/// Wraps `Arc<RwLock<SharedMarketData>>` and an optional [`StateLabel`] targeting a
/// named overlay. Call [`with_label`](Self::with_label) to obtain a labelled copy — the
/// underlying data is shared (Arc clone only, no data copy).
#[derive(Clone)]
pub struct SharedMarketDataRef {
    data: Arc<RwLock<SharedMarketData>>,
    label: Option<StateLabel>,
}

impl SharedMarketDataRef {
    /// Wraps an existing `Arc<RwLock<SharedMarketData>>` without a label.
    pub fn new(data: Arc<RwLock<SharedMarketData>>) -> Self {
        Self { data, label: None }
    }

    /// Returns a copy of this handle targeting the given labeled overlay.
    ///
    /// Arc pointer clone only — no data is copied.
    pub fn with_label(&self, label: StateLabel) -> Self {
        Self { data: Arc::clone(&self.data), label: Some(label) }
    }

    /// Acquires a read guard. If a label is set, [`MarketDataReadGuard::get_simulation_state`]
    /// checks the overlay first before falling back to the base state.
    pub async fn read(&self) -> MarketDataReadGuard<'_> {
        MarketDataReadGuard { guard: self.data.read().await, label: self.label.as_ref() }
    }

    /// Acquires an exclusive write guard.
    pub(crate) async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, SharedMarketData> {
        self.data.write().await
    }

    /// Registers (or overwrites) a simulation-state overlay for `label`.
    ///
    /// Only changed pool states need to be supplied. Missing pools fall back to the base state.
    pub async fn register_labeled_state(
        &self,
        label: StateLabel,
        states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
    ) {
        self.data
            .write()
            .await
            .register_labeled_state(label, states);
    }

    /// Removes the overlay for `label`. No-op if absent.
    pub async fn remove_labeled_state(&self, label: &StateLabel) {
        self.data
            .write()
            .await
            .remove_labeled_state(label);
    }

    /// Removes all labeled-state overlays.
    pub async fn clear_labeled_states(&self) {
        self.data
            .write()
            .await
            .clear_labeled_states();
    }

    /// Creates a new empty shared market data store wrapped in `SharedMarketDataRef`.
    pub fn new_shared() -> Self {
        Self::new(Arc::new(RwLock::new(SharedMarketData::new())))
    }

    #[cfg(test)]
    pub(crate) fn try_write_blocking(
        &self,
    ) -> Result<tokio::sync::RwLockWriteGuard<'_, SharedMarketData>, tokio::sync::TryLockError>
    {
        self.data.try_write()
    }

    #[cfg(test)]
    pub(crate) fn try_read_blocking(
        &self,
    ) -> Result<tokio::sync::RwLockReadGuard<'_, SharedMarketData>, tokio::sync::TryLockError> {
        self.data.try_read()
    }
}

/// Read guard for [`SharedMarketData`] that intercepts `get_simulation_state` to
/// check a labeled overlay first.
pub struct MarketDataReadGuard<'a> {
    guard: tokio::sync::RwLockReadGuard<'a, SharedMarketData>,
    label: Option<&'a StateLabel>,
}

impl<'a> std::ops::Deref for MarketDataReadGuard<'a> {
    type Target = SharedMarketData;

    fn deref(&self) -> &SharedMarketData {
        &self.guard
    }
}

impl<'a> MarketDataReadGuard<'a> {
    /// Looks up a simulation state, checking the labeled overlay first (if any), then the base.
    ///
    /// This method shadows [`SharedMarketData::get_simulation_state`] so algorithm call sites
    /// require no changes — they call `market.read().await.get_simulation_state(id)` as before.
    pub fn get_simulation_state(&self, id: &str) -> Option<&dyn ProtocolSim> {
        if let Some(label) = self.label {
            if let Some(state) = self
                .guard
                .get_labeled_simulation_state(label, id)
            {
                return Some(state);
            }
        }
        self.guard.get_simulation_state(id)
    }

    /// Extracts a market subset and applies the labeled overlay on top of it.
    ///
    /// Calls [`SharedMarketData::extract_subset`] for base data, then replaces any states that
    /// appear in the active overlay (if a label is set). Only states already in the subset are
    /// replaced — the overlay cannot introduce new pools.
    pub fn extract_subset_with_overlay(
        &self,
        component_ids: &HashSet<ComponentId>,
    ) -> SharedMarketData {
        let mut subset = self.guard.extract_subset(component_ids);
        if let Some(label) = self.label {
            self.guard
                .apply_overlay_to_subset(&mut subset, label);
        }
        subset
    }
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
    /// Current gas price. None if not fetched yet.
    gas_price: Option<BlockGasPrice>,
    /// Protocol sync status indexed by their protocol system name.
    protocol_sync_status: HashMap<String, SynchronizerState>,
    /// Block info for the last update (only updated when protocols reported "Ready" status).
    /// None if no block has been processed yet.
    last_updated: Option<BlockInfo>,
    /// Named simulation-state overlays. Each overlay holds only the changed pools for that label;
    /// lookups fall back to `simulation_states` for pools not present in the overlay.
    labeled_states: HashMap<StateLabel, HashMap<ComponentId, Box<dyn ProtocolSim>>>,
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
            labeled_states: HashMap::new(),
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

    /// Gets a simulation state by ID from the base state.
    ///
    /// Call sites that go through [`MarketDataReadGuard`] automatically check the labeled overlay
    /// first via the guard's own `get_simulation_state` method.
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

    // ==================== Labeled-state CRUD ====================

    /// Registers (or overwrites) a simulation-state overlay for `label`.
    ///
    /// Only changed pool states need to be supplied. Missing pools fall back to the base state.
    pub fn register_labeled_state(
        &mut self,
        label: StateLabel,
        states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
    ) {
        self.labeled_states
            .insert(label, states);
    }

    /// Removes the overlay for `label`. No-op if absent.
    pub fn remove_labeled_state(&mut self, label: &StateLabel) {
        self.labeled_states.remove(label);
    }

    /// Removes all labeled-state overlays.
    pub fn clear_labeled_states(&mut self) {
        self.labeled_states.clear();
    }

    /// Iterates over active label identifiers.
    pub fn labeled_state_ids(&self) -> impl Iterator<Item = &StateLabel> {
        self.labeled_states.keys()
    }

    /// Returns the label corresponding to the current base state (current block hash).
    ///
    /// Returns `None` only before the first block has been processed.
    pub fn current_block_label(&self) -> Option<StateLabel> {
        self.last_updated
            .as_ref()
            .map(|b| StateLabel::new(b.hash().to_string()))
    }

    /// Looks up a simulation state in the named overlay only (does not fall back to base).
    ///
    /// Called by [`MarketDataReadGuard::get_simulation_state`] before falling back to the base
    /// state.
    pub(crate) fn get_labeled_simulation_state(
        &self,
        label: &StateLabel,
        id: &str,
    ) -> Option<&dyn ProtocolSim> {
        self.labeled_states
            .get(label)?
            .get(id)
            .map(|b| b.as_ref())
    }

    /// Merges labeled overlay states into an existing subset.
    ///
    /// Only replaces states already present in the subset — no new pools are added from the
    /// overlay. This preserves the component scope determined by `extract_subset`.
    pub(crate) fn apply_overlay_to_subset(
        &self,
        subset: &mut SharedMarketData,
        label: &StateLabel,
    ) {
        let Some(overlay) = self.labeled_states.get(label) else { return };
        for (id, state) in overlay {
            if subset
                .simulation_states
                .contains_key(id)
            {
                subset
                    .simulation_states
                    .insert(id.clone(), state.clone_box());
            }
        }
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
            labeled_states: HashMap::new(), // Overlays are not needed in subsets
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

    // ==================== StateLabel tests ====================

    #[test]
    fn state_label_new_and_as_str() {
        let label = StateLabel::new("block:42");
        assert_eq!(label.as_str(), "block:42");
    }

    #[test]
    fn state_label_display() {
        let label = StateLabel::new("ephemeral:1");
        assert_eq!(label.to_string(), "ephemeral:1");
    }

    #[test]
    fn state_label_equality_and_hash() {
        let a = StateLabel::new("x");
        let b = StateLabel::new("x");
        let c = StateLabel::new("y");
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut map: HashMap<StateLabel, u32> = HashMap::new();
        map.insert(a.clone(), 1);
        assert_eq!(*map.get(&b).unwrap(), 1);
    }

    // ==================== Labeled-state CRUD tests ====================

    #[test]
    fn register_and_lookup_labeled_state() {
        let mut market = SharedMarketData::new();
        let label = StateLabel::new("ephemeral:1");
        // Use a non-zero fee so the assertion distinguishes a real lookup from a default value.
        let sim = Box::new(MockProtocolSim::new(5.0).with_fee(0.01)) as Box<dyn ProtocolSim>;

        let mut states: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        states.insert("pool_ab".to_string(), sim);
        market.register_labeled_state(label.clone(), states);

        let result = market.get_labeled_simulation_state(&label, "pool_ab");
        assert!(result.is_some());
        assert!((result.unwrap().fee() - 0.01).abs() < f64::EPSILON);
    }

    #[test]
    fn get_labeled_simulation_state_returns_none_for_missing_label() {
        let market = SharedMarketData::new();
        let label = StateLabel::new("missing");
        assert!(market
            .get_labeled_simulation_state(&label, "pool_ab")
            .is_none());
    }

    #[test]
    fn get_labeled_simulation_state_returns_none_for_missing_pool() {
        let mut market = SharedMarketData::new();
        let label = StateLabel::new("ephemeral:1");
        market.register_labeled_state(label.clone(), HashMap::new());
        assert!(market
            .get_labeled_simulation_state(&label, "pool_ab")
            .is_none());
    }

    #[test]
    fn remove_labeled_state_removes_overlay() {
        let mut market = SharedMarketData::new();
        let label = StateLabel::new("test");
        market.register_labeled_state(label.clone(), HashMap::new());
        assert_eq!(market.labeled_state_ids().count(), 1);

        market.remove_labeled_state(&label);
        assert_eq!(market.labeled_state_ids().count(), 0);
    }

    #[test]
    fn remove_labeled_state_is_noop_for_absent_label() {
        let mut market = SharedMarketData::new();
        let label = StateLabel::new("absent");
        // Should not panic
        market.remove_labeled_state(&label);
    }

    #[test]
    fn clear_labeled_states_removes_all() {
        let mut market = SharedMarketData::new();
        market.register_labeled_state(StateLabel::new("a"), HashMap::new());
        market.register_labeled_state(StateLabel::new("b"), HashMap::new());
        assert_eq!(market.labeled_state_ids().count(), 2);

        market.clear_labeled_states();
        assert_eq!(market.labeled_state_ids().count(), 0);
    }

    #[test]
    fn labeled_state_ids_iterates_registered_labels() {
        let mut market = SharedMarketData::new();
        let la = StateLabel::new("a");
        let lb = StateLabel::new("b");
        market.register_labeled_state(la.clone(), HashMap::new());
        market.register_labeled_state(lb.clone(), HashMap::new());

        let ids: HashSet<&StateLabel> = market.labeled_state_ids().collect();
        assert!(ids.contains(&la));
        assert!(ids.contains(&lb));
    }

    #[test]
    fn current_block_label_returns_none_before_first_block() {
        let market = SharedMarketData::new();
        assert!(market.current_block_label().is_none());
    }

    #[test]
    fn current_block_label_returns_hash_label_after_update() {
        let mut market = SharedMarketData::new();
        market.update_last_updated(BlockInfo::new(1, "0xdeadbeef".to_string(), 0));
        let label = market
            .current_block_label()
            .expect("should have a label");
        assert_eq!(label.as_str(), "0xdeadbeef");
    }

    // ==================== MarketDataReadGuard overlay tests ====================

    #[tokio::test]
    async fn read_guard_falls_back_to_base_when_no_label() {
        let mut market = SharedMarketData::new();
        market.update_states([(
            "pool_ab".to_string(),
            Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
        )]);
        let market_ref = SharedMarketDataRef::new(Arc::new(RwLock::new(market)));

        let guard = market_ref.read().await;
        let state = guard.get_simulation_state("pool_ab");
        assert!(state.is_some());
        // MockProtocolSim::fee() returns the fee field (0.0 by default)
        assert!((state.unwrap().fee() - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn read_guard_returns_overlay_state_when_label_matches() {
        let mut market = SharedMarketData::new();
        // Base state: fee = 0.0
        market.update_states([(
            "pool_ab".to_string(),
            Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
        )]);
        let label = StateLabel::new("test-overlay");
        // Overlay state: fee = 0.01
        let mut overlay_states: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        overlay_states.insert(
            "pool_ab".to_string(),
            Box::new(MockProtocolSim::new(3.0).with_fee(0.01)) as Box<dyn ProtocolSim>,
        );
        market.register_labeled_state(label.clone(), overlay_states);

        let market_ref = SharedMarketDataRef::new(Arc::new(RwLock::new(market)));
        let labeled_ref = market_ref.with_label(label);
        let guard = labeled_ref.read().await;
        let state = guard.get_simulation_state("pool_ab");
        assert!(state.is_some());
        // Should return overlay (fee 0.01), not base (fee 0.0)
        assert!((state.unwrap().fee() - 0.01).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn read_guard_falls_back_to_base_when_pool_not_in_overlay() {
        let mut market = SharedMarketData::new();
        market.update_states([(
            "pool_ab".to_string(),
            Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
        )]);
        let label = StateLabel::new("test-overlay");
        // Overlay exists but does not include pool_ab
        market.register_labeled_state(label.clone(), HashMap::new());

        let market_ref = SharedMarketDataRef::new(Arc::new(RwLock::new(market)));
        let labeled_ref = market_ref.with_label(label);
        let guard = labeled_ref.read().await;
        let state = guard.get_simulation_state("pool_ab");
        // Should fall back to base state
        assert!(state.is_some());
        assert!((state.unwrap().fee() - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn with_label_shares_underlying_data() {
        let market_ref = SharedMarketDataRef::new_shared();
        let label = StateLabel::new("lbl");
        let labeled_ref = market_ref.with_label(label);

        // Both refs share the same Arc
        {
            let mut guard = market_ref.write().await;
            guard.update_last_updated(BlockInfo::new(1, "hash".to_string(), 0));
        }
        let read = labeled_ref.read().await;
        assert!(read.last_updated().is_some());
    }
}
