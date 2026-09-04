//! Shared market data structure.
//!
//! This is the single source of truth for all market data.
//! It's protected by a RwLock and shared across all components:
//! - TychoIndexer: WRITE access to update data
//! - Solvers: READ access to query states during solving
//!
//! We use tokio RwLock (which is write-preferring) to avoid writer starvation.
//!
//! # Overlay design
//!
//! Labeled overlay states (used by solver components to inject per-request component states) are
//! stored in a separate `Arc<RwLock<...>>` on `MarketData` rather than inside the main
//! `MarketState` lock. This decouples overlay writes from base-state reads: a TychoFeed block
//! update no longer stalls overlay registrations and vice versa.

use std::sync::Arc;

use num_bigint::BigUint;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::RwLock;
use tycho_simulation::{
    tycho_client::feed::SynchronizerState,
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Address},
        simulation::protocol_sim::ProtocolSim,
    },
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

use crate::types::{BlockInfo, ComponentId};

/// A label identifying an overlay state layer.
///
/// Each labeled overlay is an independent snapshot of component states that can be layered
/// on top of the base market state for a specific worker component or request context.
pub type StateLabel = String;

/// An immutable snapshot of per-component simulation states for one overlay layer.
pub type OverlayStates = Arc<FxHashMap<ComponentId, Box<dyn ProtocolSim>>>;

/// A named simulation-state overlay with a block-number expiry.
pub struct OverlayEntry {
    /// The overlay component states (only components that differ from base state).
    pub states: OverlayStates,
    /// Last block number for which this overlay is valid.
    /// The overlay is automatically evicted before block `valid_until + 1` is applied.
    pub valid_until: u64,
}

/// The shared overlay registry: maps each label to its snapshot.
type OverlayRegistry = Arc<RwLock<FxHashMap<StateLabel, OverlayEntry>>>;

/// Error returned by [`MarketData::read_labeled`] when the requested label cannot be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ReadLabeledError {
    /// The label is not registered as an overlay and does not match the current base-state label.
    #[error("label not found: {0}")]
    NotFound(StateLabel),
}

/// The main entry point for accessing market data.
///
/// Cloning is cheap — all clones share the same underlying data and overlay registry.
/// Pass an optional label to `read` to scope the view to a specific overlay.
#[derive(Clone)]
pub struct MarketData {
    data: Arc<RwLock<MarketState>>,
    /// Per-label overlay states. Stored separately from the base data lock so that
    /// overlay writes do not block base-state reads.
    overlays: OverlayRegistry,
    /// Gas price this handle reports instead of the one on the shared state, in wei per gas unit.
    ///
    /// Set by [`MarketData::with_gas_price_override`]. It lives on the handle, not on the shared
    /// state, so one request can solve at a different gas price without any other request seeing
    /// it.
    gas_price_override: Option<Arc<BigUint>>,
}

impl MarketData {
    /// Creates a new handle wrapping the given data store.
    pub fn new(data: Arc<RwLock<MarketState>>) -> Self {
        Self {
            data,
            overlays: Arc::new(RwLock::new(FxHashMap::default())),
            gas_price_override: None,
        }
    }

    /// Creates a new empty market data store wrapped in a `MarketData`.
    pub fn new_shared() -> Self {
        Self::new(Arc::new(RwLock::new(MarketState::new())))
    }

    /// A handle over the same shared state and overlays whose views report `wei` as the gas
    /// price, in wei per gas unit.
    ///
    /// Cloning is as cheap as cloning any other handle. The shared state is not touched, so
    /// handles held elsewhere keep reporting the gas price the feed wrote. The shadowed price
    /// keeps the block number, hash and timestamp of the price it replaces, so staleness checks
    /// still read the real block.
    ///
    /// The override replaces the feed's price; it does not stand in for one. While the feed has
    /// not written a price, views of this handle report none, exactly like every other handle, so
    /// a request carrying an override cannot solve while one without it cannot.
    #[must_use]
    pub fn with_gas_price_override(&self, wei: BigUint) -> Self {
        Self {
            data: Arc::clone(&self.data),
            overlays: Arc::clone(&self.overlays),
            gas_price_override: Some(Arc::new(wei)),
        }
    }

    /// Builds a view over `guard`, applying this handle's gas price override if it has one.
    fn view<'a>(
        &self,
        guard: tokio::sync::RwLockReadGuard<'a, MarketState>,
        overlay: Option<(StateLabel, OverlayStates)>,
    ) -> MarketDataView<'a> {
        let gas_price_shadow = self
            .gas_price_override
            .as_ref()
            .and_then(|wei| Some(shadow_gas_price(guard.gas_price()?, wei)));
        MarketDataView { guard, overlay, gas_price_shadow }
    }

    /// Acquires a base view of the market data with no overlay applied.
    pub async fn read(&self) -> MarketDataView<'_> {
        let guard = self.data.read().await;
        self.view(guard, None)
    }

    /// Acquires an overlay-aware view scoped to `label`.
    ///
    /// Succeeds when `label` is registered as an overlay **or** matches the current base-state
    /// label (the block-number string set by `apply_block_update`). Returns
    /// [`ReadLabeledError::NotFound`] otherwise so callers cannot silently fall back to stale data.
    ///
    /// The overlay lock is held only briefly to clone the snapshot pointer; it is released
    /// before the view is returned, so solving never holds two locks simultaneously.
    pub async fn read_labeled(
        &self,
        label: &StateLabel,
    ) -> Result<MarketDataView<'_>, ReadLabeledError> {
        let guard = self.data.read().await;
        if let Some(e) = self.overlays.read().await.get(label) {
            let states = Arc::clone(&e.states);
            return Ok(self.view(guard, Some((label.clone(), states))));
        }
        if &guard.label == label {
            return Ok(self.view(guard, None));
        }
        Err(ReadLabeledError::NotFound(label.clone()))
    }

    /// Acquires an exclusive write guard on the base data store.
    pub async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, MarketState> {
        self.data.write().await
    }

    /// Attempts a non-blocking read of the base data store.
    ///
    /// Returns `None` if the lock is currently held for writing.
    pub fn try_read(&self) -> Option<tokio::sync::RwLockReadGuard<'_, MarketState>> {
        self.data.try_read().ok()
    }

    /// Attempts a non-blocking write lock on the base data store.
    ///
    /// Returns `None` if the lock is currently held for reading or writing.
    pub fn try_write(&self) -> Option<tokio::sync::RwLockWriteGuard<'_, MarketState>> {
        self.data.try_write().ok()
    }

    /// Attempts a non-blocking read and wraps the result in a `MarketDataView`.
    ///
    /// The overlay is not applied, so this only exposes the base market state. Suitable for
    /// callers that read base data (e.g. token decimals) and do not depend on overlay state,
    /// such as the quote price-impact fallback. Returns `None` if the lock is currently held
    /// for writing (callers must treat that as "data unavailable", not an error).
    pub fn try_read_blocking(&self) -> Option<MarketDataView<'_>> {
        self.data
            .try_read()
            .ok()
            .map(|guard| self.view(guard, None))
    }

    // ==================== Overlay CRUD ====================

    /// Registers or replaces an overlay for the given label.
    pub async fn register_labeled_state(
        &self,
        label: StateLabel,
        states: FxHashMap<ComponentId, Box<dyn ProtocolSim>>,
        valid_until: u64,
    ) {
        self.overlays
            .write()
            .await
            .insert(label, OverlayEntry { states: Arc::new(states), valid_until });
    }

    /// Removes the overlay for the given label, if it exists.
    pub async fn remove_labeled_state(&self, label: &StateLabel) {
        self.overlays
            .write()
            .await
            .remove(label);
    }

    /// Clears all overlays.
    pub async fn clear_labeled_states(&self) {
        self.overlays.write().await.clear();
    }

    /// Atomically evicts stale overlays then applies a block update to base state.
    ///
    /// Overlays with `valid_until < new_block_number` are removed under the overlay
    /// lock before the base write lock is acquired. This guarantees no solver can
    /// observe new base state alongside an overlay that was built against the previous
    /// block.
    pub async fn apply_block_update(
        &self,
        new_block_number: u64,
        update: impl FnOnce(&mut MarketState),
    ) {
        self.overlays
            .write()
            .await
            .retain(|_, entry| entry.valid_until >= new_block_number);
        let mut data = self.data.write().await;
        data.label = new_block_number.to_string();
        update(&mut data);
    }

    /// Returns the labels of all registered overlays.
    pub async fn labeled_state_ids(&self) -> Vec<StateLabel> {
        self.overlays
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }
}

/// An overlay-aware view of the market data, held for the duration of a read lock.
///
/// Holds a read lock on the base `MarketState` and an optional overlay snapshot.
/// Use `get_simulation_state` for overlay-aware component lookups. All other accessors
/// delegate to the base data.
pub struct MarketDataView<'a> {
    guard: tokio::sync::RwLockReadGuard<'a, MarketState>,
    overlay: Option<(StateLabel, OverlayStates)>,
    /// Gas price this view reports in place of the base one, set when the handle it came from
    /// carries an override. Built once per view so `gas_price` can hand out a reference.
    gas_price_shadow: Option<BlockGasPrice>,
}

/// Builds the gas price a view reports when its handle overrides the price, keeping the block
/// provenance of `base` so staleness checks still read the real block.
fn shadow_gas_price(base: &BlockGasPrice, wei: &BigUint) -> BlockGasPrice {
    BlockGasPrice { pricing: GasPrice::Legacy { gas_price: wei.clone() }, ..base.clone() }
}

impl<'a> MarketDataView<'a> {
    /// Returns the label identifying the active overlay, or `None` if no overlay is in effect.
    pub fn state_label(&self) -> Option<&StateLabel> {
        self.overlay
            .as_ref()
            .map(|(label, _)| label)
    }

    /// Returns the simulation state for the given component, checking the overlay first.
    pub fn get_simulation_state(&self, id: &str) -> Option<&dyn ProtocolSim> {
        if let Some((_, ref states)) = self.overlay {
            if let Some(s) = states.get(id) {
                return Some(s.as_ref());
            }
        }
        self.guard.get_simulation_state(id)
    }

    /// Extracts a base-data subset for the given component IDs, then layers the active overlay
    /// on top by replacing any simulation states found in both the subset and the overlay.
    ///
    /// If no overlay is active, this is equivalent to `self.extract_subset(component_ids)`.
    pub fn extract_subset_with_overlay(
        &self,
        component_ids: &FxHashSet<&ComponentId>,
    ) -> MarketState {
        let mut subset = self.guard.extract_subset(component_ids);
        if let Some(shadow) = self.gas_price_shadow.clone() {
            subset.update_gas_price(shadow);
        }
        if let Some((ref label, ref states)) = self.overlay {
            for (id, state) in states.iter() {
                if subset
                    .simulation_states
                    .contains_key(id)
                {
                    subset
                        .simulation_states
                        .insert(id.clone(), state.clone_box());
                }
            }
            subset.label = label.clone();
        }
        subset
    }

    /// Returns the component topology from the base data.
    pub fn component_topology(&self) -> FxHashMap<ComponentId, Vec<Address>> {
        self.guard.component_topology()
    }

    /// Extracts a base-data subset for the given component IDs (no overlay applied).
    ///
    /// The handle's gas price override, when it has one, still applies: an algorithm prices a
    /// route off the subset it extracts, so leaving the base price on it would drop the override.
    pub fn extract_subset(&self, component_ids: &FxHashSet<&ComponentId>) -> MarketState {
        let mut subset = self.guard.extract_subset(component_ids);
        if let Some(shadow) = self.gas_price_shadow.clone() {
            subset.update_gas_price(shadow);
        }
        subset
    }

    /// Returns a reference to the token registry from the base data.
    pub fn token_registry_ref(&self) -> &FxHashMap<Address, Arc<Token>> {
        self.guard.token_registry_ref()
    }

    /// Returns the gas price this view solves at: the handle's override when it carries one,
    /// and the base data's price otherwise.
    pub fn gas_price(&self) -> Option<&BlockGasPrice> {
        self.gas_price_shadow
            .as_ref()
            .or_else(|| self.guard.gas_price())
    }

    /// Returns the block info for the last base-state update.
    pub fn last_updated(&self) -> Option<&BlockInfo> {
        self.guard.last_updated()
    }

    /// Returns a token by address from the base data.
    pub fn get_token(&self, address: &Address) -> Option<&Token> {
        self.guard.get_token(address)
    }

    /// Returns a token by address from the base data, to be held rather than copied.
    pub fn get_token_shared(&self, address: &Address) -> Option<&Arc<Token>> {
        self.guard.get_token_shared(address)
    }

    /// Returns a component by ID from the base data.
    pub fn get_component(&self, id: &str) -> Option<&ProtocolComponent> {
        self.guard.get_component(id)
    }

    /// Returns a reference to the underlying base market state, bypassing any overlay.
    pub fn base_market_state(&self) -> &MarketState {
        &self.guard
    }
}

/// Shared market data containing all component states and market information.
///
/// This struct is the single source of truth for market data.
/// The indexer updates it, and solvers read from it.
#[derive(Debug, Default)]
pub struct MarketState {
    /// Identifies the block or overlay this state was produced from.
    ///
    /// Set to the block number string by `apply_block_update`; copied from the overlay label by
    /// `extract_subset_with_overlay` when an overlay is active. Empty string until the first block
    /// is applied.
    label: StateLabel,
    /// All components indexed by their ID.
    components: FxHashMap<ComponentId, Arc<ProtocolComponent>>,
    /// All states indexed by their component ID.
    simulation_states: FxHashMap<ComponentId, Box<dyn ProtocolSim>>,
    /// All tokens indexed by their address. Shared for the same reason as `components`.
    tokens: FxHashMap<Address, Arc<Token>>,
    /// Current gas price. None if not fetched yet.
    gas_price: Option<BlockGasPrice>,
    /// Protocol sync status indexed by their protocol system name.
    protocol_sync_status: FxHashMap<String, SynchronizerState>,
    /// Block info for the last update (only updated when protocols reported "Ready" status).
    /// None if no block has been processed yet.
    last_updated: Option<BlockInfo>,
    /// Number of components per protocol system, maintained incrementally on
    /// upsert/remove so readers never scan the full component map.
    component_counts: FxHashMap<String, u64>,
}

impl MarketState {
    /// Creates a new empty MarketState.
    pub fn new() -> Self {
        Self {
            label: String::new(),
            components: FxHashMap::default(),
            simulation_states: FxHashMap::default(),
            tokens: FxHashMap::default(),
            gas_price: None,
            protocol_sync_status: FxHashMap::default(),
            last_updated: None,
            component_counts: FxHashMap::default(),
        }
    }

    /// Returns the label identifying the block or overlay this state was produced from.
    pub fn label(&self) -> &StateLabel {
        &self.label
    }

    /// Returns the block info for the last update.
    pub fn last_updated(&self) -> Option<&BlockInfo> {
        self.last_updated.as_ref()
    }

    /// Number of protocol components (components) currently tracked.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Number of tokens currently tracked.
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Number of components (components) per protocol system.
    ///
    /// Entries stay present at zero after all of a protocol's components are
    /// removed so exported gauges reset instead of freezing at the last value.
    pub fn component_counts_by_protocol(&self) -> &FxHashMap<String, u64> {
        &self.component_counts
    }

    /// Returns the sync status of every protocol system.
    pub fn protocol_sync_states(&self) -> &FxHashMap<String, SynchronizerState> {
        &self.protocol_sync_status
    }

    /// Returns the protocol sync status indexed by their protocol system name.
    pub fn get_protocol_sync_status(&self, protocol_system: &String) -> Option<&SynchronizerState> {
        self.protocol_sync_status
            .get(protocol_system)
    }

    /// Returns the component topology.
    /// This is a simple mapping from component ID to their token addresses.
    pub fn component_topology(&self) -> FxHashMap<ComponentId, Vec<Address>> {
        self.components
            .iter()
            .map(|(id, component)| (id.clone(), component.tokens.clone()))
            .collect()
    }

    /// Gets a component by ID.
    pub fn get_component(&self, id: &str) -> Option<&ProtocolComponent> {
        self.components.get(id).map(Arc::as_ref)
    }

    /// Gets a component by ID as a shared handle, for callers that need to keep it.
    pub fn get_component_shared(&self, id: &str) -> Option<&Arc<ProtocolComponent>> {
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
        self.tokens
            .get(address)
            .map(Arc::as_ref)
    }

    /// Gets a token as a shared handle, for callers that need to keep it.
    pub fn get_token_shared(&self, address: &Address) -> Option<&Arc<Token>> {
        self.tokens.get(address)
    }

    /// Returns the current gas price. None if not fetched yet.
    pub fn gas_price(&self) -> Option<&BlockGasPrice> {
        self.gas_price.as_ref()
    }

    /// Returns a reference to the token registry.
    pub fn token_registry_ref(&self) -> &FxHashMap<Address, Arc<Token>> {
        &self.tokens
    }

    /// Inserts or updates a component.
    pub fn upsert_components(&mut self, components: impl IntoIterator<Item = ProtocolComponent>) {
        for component in components {
            let protocol_system = component.protocol_system.clone();
            let previous = self
                .components
                .insert(component.id.clone(), Arc::new(component));
            if previous.is_none() {
                *self
                    .component_counts
                    .entry(protocol_system)
                    .or_default() += 1;
            }
        }
    }

    /// Inserts or updates tokens.
    pub fn upsert_tokens(&mut self, tokens: impl IntoIterator<Item = Token>) {
        for token in tokens {
            self.tokens
                .insert(token.address.clone(), Arc::new(token));
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
            if let Some(component) = self.components.remove(id) {
                if let Some(count) = self
                    .component_counts
                    .get_mut(&component.protocol_system)
                {
                    *count = count.saturating_sub(1);
                }
            }
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

    /// Creates a filtered subset containing only data needed for the given components.
    ///
    /// This is used to create a local snapshot of market data that can be used for
    /// simulation without holding the main lock. The subset includes:
    /// - Components matching the provided IDs
    /// - Simulation states for those components (cloned via `clone_box`)
    /// - Tokens referenced by those components
    /// - Gas price and block info
    pub fn extract_subset(&self, component_ids: &FxHashSet<&ComponentId>) -> MarketState {
        let mut components =
            FxHashMap::with_capacity_and_hasher(component_ids.len(), rustc_hash::FxBuildHasher);
        let mut simulation_states =
            FxHashMap::with_capacity_and_hasher(component_ids.len(), rustc_hash::FxBuildHasher);
        // Tokens are shared between components, so this collects addresses first and resolves
        // them once each rather than per component that mentions them.
        let mut token_addresses: FxHashSet<&Address> =
            FxHashSet::with_capacity_and_hasher(component_ids.len() * 2, rustc_hash::FxBuildHasher);

        for &id in component_ids {
            if let Some(component) = self.components.get(id) {
                token_addresses.extend(&component.tokens);
                components.insert(id.clone(), component.clone());
            }
            // A component without a simulation state is legitimate: the recording skips `vm:*`
            // states, and a component can be announced a block before its first state arrives.
            if let Some(state) = self.simulation_states.get(id) {
                simulation_states.insert(id.clone(), state.clone_box());
            }
        }

        let mut tokens =
            FxHashMap::with_capacity_and_hasher(token_addresses.len(), rustc_hash::FxBuildHasher);
        for address in token_addresses {
            if let Some(token) = self.tokens.get(address) {
                tokens.insert(address.clone(), token.clone());
            }
        }

        MarketState {
            label: self.label.clone(),
            components,
            simulation_states,
            tokens,
            gas_price: self.gas_price.clone(),
            protocol_sync_status: FxHashMap::default(), // Not needed for simulation
            last_updated: self.last_updated.clone(),
            component_counts: FxHashMap::default(), // Not needed for simulation
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;
    use crate::algorithm::test_utils::{
        component, component_with_protocol, token, MockProtocolSim,
    };

    #[test]
    fn component_counts_by_protocol_tracks_upserts_and_removals() {
        let mut market = MarketState::new();
        let component_tokens = [token(0x0A, "A"), token(0x0B, "B")];

        market.upsert_components([
            component_with_protocol("component_1", "uniswap_v2", &component_tokens),
            component_with_protocol("component_2", "uniswap_v2", &component_tokens),
            component_with_protocol("component_3", "uniswap_v3", &component_tokens),
        ]);
        let counts = market.component_counts_by_protocol();
        assert_eq!(counts.get("uniswap_v2"), Some(&2));
        assert_eq!(counts.get("uniswap_v3"), Some(&1));

        // Re-upserting an existing component is an update, not a new component.
        market.upsert_components([component_with_protocol(
            "component_1",
            "uniswap_v2",
            &component_tokens,
        )]);
        assert_eq!(
            market
                .component_counts_by_protocol()
                .get("uniswap_v2"),
            Some(&2)
        );

        // Removals decrement; the entry stays at zero so exported gauges reset
        // instead of freezing at the last non-zero value.
        let removed_ids = ["component_1".to_string(), "component_3".to_string()];
        market.remove_components(removed_ids.iter());
        let counts = market.component_counts_by_protocol();
        assert_eq!(counts.get("uniswap_v2"), Some(&1));
        assert_eq!(counts.get("uniswap_v3"), Some(&0));

        // Removing an unknown id leaves counts untouched.
        let unknown_ids = ["unknown_component".to_string()];
        market.remove_components(unknown_ids.iter());
        assert_eq!(
            market
                .component_counts_by_protocol()
                .get("uniswap_v2"),
            Some(&1)
        );
    }

    #[test]
    fn extract_subset_filters_by_component_ids() {
        // Setup: market with 2 components (A-B, B-C) and 3 tokens
        let mut market = MarketState::new();

        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");

        market.upsert_components([
            component("component_ab", &[token_a.clone(), token_b.clone()]),
            component("component_bc", &[token_b.clone(), token_c.clone()]),
        ]);
        market.upsert_tokens([token_a.clone(), token_b.clone(), token_c.clone()]);
        market.update_states([
            (
                "component_ab".to_string(),
                Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
            ),
            (
                "component_bc".to_string(),
                Box::new(MockProtocolSim::new(3.0)) as Box<dyn ProtocolSim>,
            ),
        ]);
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });
        market.update_last_updated(BlockInfo::new(12345, "0xabc".to_string(), 0));

        // Extract only component_ab
        let component_ab = "component_ab".to_string();
        let ids: FxHashSet<&ComponentId> = [&component_ab].into_iter().collect();
        let subset = market.extract_subset(&ids);

        // Components: only component_ab
        assert_eq!(subset.components.len(), 1);
        assert!(subset
            .components
            .contains_key("component_ab"));

        // Tokens: only A and B (referenced by component_ab), not C
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

        // Simulation states: only component_ab
        assert_eq!(subset.simulation_states.len(), 1);
        assert!(subset
            .simulation_states
            .contains_key("component_ab"));

        // Gas price and block info are copied
        assert_eq!(subset.gas_price, market.gas_price);
        assert!(subset.last_updated.is_some());

        // Empty IDs returns empty subset
        let empty_subset = market.extract_subset(&FxHashSet::default());
        assert!(empty_subset.components.is_empty());
        assert!(empty_subset.tokens.is_empty());
        assert!(empty_subset
            .simulation_states
            .is_empty());
    }

    // ==================== MarketData overlay tests ====================

    #[tokio::test]
    async fn register_and_retrieve_overlay_via_labeled_read() {
        let market_ref = MarketData::new_shared();

        let label = "test_label".to_string();
        let mut states: FxHashMap<ComponentId, Box<dyn ProtocolSim>> = FxHashMap::default();
        states.insert(
            "component_ab".to_string(),
            Box::new(MockProtocolSim::new(99.0)) as Box<dyn ProtocolSim>,
        );

        market_ref
            .register_labeled_state(label.clone(), states, u64::MAX)
            .await;

        let guard = market_ref
            .read_labeled(&label)
            .await
            .expect("label was just registered");
        // Base data is empty — overlay provides the state
        let sim = guard.get_simulation_state("component_ab");
        assert!(sim.is_some());
    }

    #[tokio::test]
    async fn read_without_label_returns_no_overlay() {
        let market_ref = MarketData::new_shared();

        market_ref
            .register_labeled_state(
                "my_label".to_string(),
                FxHashMap::from_iter([(
                    "component1".to_string(),
                    Box::new(MockProtocolSim::new(5.0)) as Box<dyn ProtocolSim>,
                )]),
                u64::MAX,
            )
            .await;

        // A handle with no label must not see the overlay
        let guard = market_ref.read().await;
        assert!(guard
            .get_simulation_state("component1")
            .is_none());
    }

    #[tokio::test]
    async fn remove_labeled_state_clears_overlay() {
        let market_ref = MarketData::new_shared();
        let label = "lbl".to_string();

        market_ref
            .register_labeled_state(
                label.clone(),
                FxHashMap::from_iter([(
                    "component".to_string(),
                    Box::new(MockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
                )]),
                u64::MAX,
            )
            .await;

        market_ref
            .remove_labeled_state(&label)
            .await;

        let ids = market_ref.labeled_state_ids().await;
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn clear_labeled_states_removes_all() {
        let market_ref = MarketData::new_shared();

        for i in 0..3u8 {
            market_ref
                .register_labeled_state(
                    format!("label_{i}"),
                    FxHashMap::from_iter([(
                        format!("component_{i}"),
                        Box::new(MockProtocolSim::new(f64::from(i))) as Box<dyn ProtocolSim>,
                    )]),
                    u64::MAX,
                )
                .await;
        }

        market_ref.clear_labeled_states().await;
        assert!(market_ref
            .labeled_state_ids()
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn clone_shares_overlay_registry() {
        // Registering via one clone must be visible when reading via any other clone pointing at
        // the same overlay registry.
        let base = MarketData::new_shared();
        let clone_a = base.clone();
        let clone_b = base.clone();

        base.register_labeled_state(
            "shared".to_string(),
            FxHashMap::from_iter([(
                "component_x".to_string(),
                Box::new(MockProtocolSim::new(7.0)) as Box<dyn ProtocolSim>,
            )]),
            u64::MAX,
        )
        .await;

        let label = "shared".to_string();
        let guard_a = clone_a
            .read_labeled(&label)
            .await
            .expect("label was just registered");
        assert!(guard_a
            .get_simulation_state("component_x")
            .is_some());
        drop(guard_a);

        let guard_b = clone_b
            .read_labeled(&label)
            .await
            .expect("label was just registered");
        assert!(guard_b
            .get_simulation_state("component_x")
            .is_some());
    }

    #[tokio::test]
    async fn extract_subset_with_overlay_replaces_matching_states() {
        use crate::algorithm::test_utils::{component as mk_component, token as mk_token};

        let market_ref = MarketData::new_shared();

        let tok_a = mk_token(0x01, "A");
        let tok_b = mk_token(0x02, "B");

        {
            let mut data = market_ref.write().await;
            data.upsert_components([mk_component("component_ab", &[tok_a.clone(), tok_b.clone()])]);
            data.upsert_tokens([tok_a.clone(), tok_b.clone()]);
            data.update_states([(
                "component_ab".to_string(),
                Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
            )]);
        }

        let label = "overlay".to_string();
        market_ref
            .register_labeled_state(
                label.clone(),
                FxHashMap::from_iter([(
                    "component_ab".to_string(),
                    Box::new(MockProtocolSim::new(99.0)) as Box<dyn ProtocolSim>,
                )]),
                u64::MAX,
            )
            .await;

        let guard = market_ref
            .read_labeled(&label)
            .await
            .expect("label was just registered");
        let component_ab = "component_ab".to_string();
        let ids: FxHashSet<&ComponentId> = [&component_ab].into_iter().collect();
        let subset = guard.extract_subset_with_overlay(&ids);

        let sim = subset
            .get_simulation_state("component_ab")
            .unwrap();
        let mock = sim
            .as_any()
            .downcast_ref::<MockProtocolSim>()
            .unwrap();
        assert_eq!(mock.spot_price, 99.0, "overlay state should replace base state");
    }

    #[tokio::test]
    async fn apply_block_update_evicts_stale_overlays() {
        let market_ref = MarketData::new_shared();

        // Register two overlays: one valid until block 10, one valid until block 20.
        market_ref
            .register_labeled_state(
                "stale".to_string(),
                FxHashMap::from_iter([(
                    "component_stale".to_string(),
                    Box::new(MockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
                )]),
                10,
            )
            .await;
        market_ref
            .register_labeled_state(
                "fresh".to_string(),
                FxHashMap::from_iter([(
                    "component_fresh".to_string(),
                    Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
                )]),
                20,
            )
            .await;

        // Apply block 11: the "stale" overlay (valid_until=10) must be evicted.
        market_ref
            .apply_block_update(11, |_data| {})
            .await;

        let ids = market_ref.labeled_state_ids().await;
        assert!(!ids.contains(&"stale".to_string()), "stale overlay must be evicted");
        assert!(ids.contains(&"fresh".to_string()), "fresh overlay must survive");
    }

    #[tokio::test]
    async fn apply_block_update_applies_mutation() {
        let market_ref = MarketData::new_shared();

        market_ref
            .apply_block_update(1, |data| {
                data.update_last_updated(BlockInfo::new(1, "0xabc".to_string(), 0));
            })
            .await;

        let guard = market_ref.read().await;
        assert_eq!(
            guard
                .last_updated()
                .expect("last_updated must be set")
                .number(),
            1
        );
    }

    #[tokio::test]
    async fn component_and_token_counts_track_upserts_and_removals() {
        let market = MarketData::new_shared();
        let tok_a = token(1, "A");
        let tok_b = token(2, "B");

        market
            .apply_block_update(1, |data| {
                data.upsert_components([component(
                    "component_ab",
                    &[tok_a.clone(), tok_b.clone()],
                )]);
                data.upsert_tokens([tok_a.clone(), tok_b.clone()]);
            })
            .await;
        {
            let data = market.read().await;
            assert_eq!(
                data.base_market_state()
                    .component_count(),
                1
            );
            assert_eq!(data.base_market_state().token_count(), 2);
        }

        market
            .apply_block_update(2, |data| {
                data.remove_components(["component_ab".to_string()].iter());
            })
            .await;
        let data = market.read().await;
        assert_eq!(
            data.base_market_state()
                .component_count(),
            0
        );
        assert_eq!(
            data.base_market_state().token_count(),
            2,
            "tokens are not removed with their components"
        );
    }

    /// Writes a legacy gas price of `wei` at block 42 into the shared state.
    async fn set_base_gas_price(market: &MarketData, wei: u64) {
        market
            .write()
            .await
            .update_gas_price(BlockGasPrice {
                block_number: 42,
                block_hash: B256::repeat_byte(0x11),
                block_timestamp: 1_700_000_000,
                pricing: GasPrice::Legacy { gas_price: BigUint::from(wei) },
            });
    }

    #[tokio::test]
    async fn view_gas_price_prefers_the_handle_override() {
        let market = MarketData::new_shared();
        set_base_gas_price(&market, 50).await;

        let base_view = market.read().await;
        assert_eq!(
            base_view
                .gas_price()
                .map(BlockGasPrice::effective_gas_price),
            Some(BigUint::from(50u64)),
            "a handle without an override reports the price the feed wrote"
        );
        drop(base_view);

        let shadowed = market.with_gas_price_override(BigUint::from(5u64));
        let shadow_view = shadowed.read().await;
        assert_eq!(
            shadow_view
                .gas_price()
                .map(BlockGasPrice::effective_gas_price),
            Some(BigUint::from(5u64))
        );
        let shadow_price = shadow_view
            .gas_price()
            .expect("the override always reports a price");
        assert_eq!(
            shadow_price.block_number, 42,
            "the shadow keeps the block the real price was read at"
        );
        assert_eq!(shadow_price.block_timestamp, 1_700_000_000);
    }

    #[tokio::test]
    async fn gas_price_override_leaves_other_handles_untouched() {
        let market = MarketData::new_shared();
        set_base_gas_price(&market, 50).await;
        let sibling = market.clone();

        let shadowed = market.with_gas_price_override(BigUint::from(1u64));
        assert_eq!(
            shadowed
                .read()
                .await
                .gas_price()
                .map(BlockGasPrice::effective_gas_price),
            Some(BigUint::from(1u64))
        );

        for (name, handle) in [("origin", &market), ("sibling", &sibling)] {
            assert_eq!(
                handle
                    .read()
                    .await
                    .gas_price()
                    .map(BlockGasPrice::effective_gas_price),
                Some(BigUint::from(50u64)),
                "{name} shares the state but not the override"
            );
        }
    }

    #[tokio::test]
    async fn extracted_subsets_carry_the_gas_price_override() {
        // Algorithms price routes off the subset they extract, not off the view, so the override
        // has to survive extraction or it does nothing.
        let market = MarketData::new_shared();
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        {
            let mut data = market.write().await;
            data.upsert_components([component("component_ab", &[tok_a.clone(), tok_b.clone()])]);
            data.upsert_tokens([tok_a, tok_b]);
        }
        set_base_gas_price(&market, 50).await;

        let shadowed = market.with_gas_price_override(BigUint::from(5u64));
        let view = shadowed.read().await;
        let id = "component_ab".to_string();
        let ids = FxHashSet::from_iter([&id]);

        for (name, subset) in [
            ("extract_subset", view.extract_subset(&ids)),
            ("extract_subset_with_overlay", view.extract_subset_with_overlay(&ids)),
        ] {
            assert_eq!(
                subset
                    .gas_price()
                    .map(BlockGasPrice::effective_gas_price),
                Some(BigUint::from(5u64)),
                "{name} dropped the override"
            );
        }
    }

    #[tokio::test]
    async fn gas_price_override_needs_a_feed_price() {
        // The override replaces the feed's price; it does not stand in for one. A request that
        // carries an override must fail the same way as any other while the feed has no price.
        let market = MarketData::new_shared();
        let shadowed = market.with_gas_price_override(BigUint::from(7u64));

        assert!(
            shadowed
                .read()
                .await
                .gas_price()
                .is_none(),
            "an override without a feed price reports no price"
        );
        let subset = shadowed
            .read()
            .await
            .extract_subset(&FxHashSet::default());
        assert!(subset.gas_price().is_none(), "the subset reports no price either");
    }
}
