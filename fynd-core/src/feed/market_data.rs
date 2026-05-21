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
//! Labeled overlay states (used by solver pools to inject per-request pool states) are stored in a
//! separate `Arc<RwLock<...>>` on `MarketData` rather than inside the main
//! `MarketState` lock. This decouples overlay writes from base-state reads: a TychoFeed block
//! update no longer stalls overlay registrations and vice versa.

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

/// A label identifying an overlay state layer.
///
/// Each labeled overlay is an independent snapshot of pool states that can be layered
/// on top of the base market state for a specific worker pool or request context.
pub type StateLabel = String;

/// An immutable snapshot of per-component simulation states for one overlay layer.
pub type OverlayStates = Arc<HashMap<ComponentId, Box<dyn ProtocolSim>>>;

/// A named simulation-state overlay with a block-number expiry.
pub struct OverlayEntry {
    /// The overlay pool states (only pools that differ from base state).
    pub states: OverlayStates,
    /// Last block number for which this overlay is valid.
    /// The overlay is automatically evicted before block `valid_until + 1` is applied.
    pub valid_until: u64,
}

/// The shared overlay registry: maps each label to its snapshot.
type OverlayRegistry = Arc<RwLock<HashMap<StateLabel, OverlayEntry>>>;

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
}

impl MarketData {
    /// Creates a new handle wrapping the given data store.
    pub fn new(data: Arc<RwLock<MarketState>>) -> Self {
        Self { data, overlays: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Creates a new empty market data store wrapped in a `MarketData`.
    pub fn new_shared() -> Self {
        Self::new(Arc::new(RwLock::new(MarketState::new())))
    }

    /// Acquires an overlay-aware view of the market data.
    ///
    /// If `label` is `Some`, the view will expose overlay states registered under that label,
    /// falling back to base state for pools not present in the overlay.
    /// The overlay lock is held only briefly to clone the snapshot pointer; it is released
    /// before the view is returned, so solving never holds two locks simultaneously.
    pub async fn read(&self, label: Option<&StateLabel>) -> MarketDataView<'_> {
        let guard = self.data.read().await;
        let overlay = if let Some(label) = label {
            self.overlays
                .read()
                .await
                .get(label)
                .map(|e| (label.clone(), Arc::clone(&e.states)))
        } else {
            None
        };
        MarketDataView { guard, overlay }
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
    /// The overlay is not applied because this is a synchronous helper intended for tests where
    /// no overlay is active.  Returns `None` if the lock is currently held for writing.
    pub fn try_read_blocking(&self) -> Option<MarketDataView<'_>> {
        self.data
            .try_read()
            .ok()
            .map(|guard| MarketDataView { guard, overlay: None })
    }

    // ==================== Overlay CRUD ====================

    /// Registers or replaces an overlay for the given label.
    pub async fn register_labeled_state(
        &self,
        label: StateLabel,
        states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
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
        update(&mut *self.data.write().await);
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
/// Use `get_simulation_state` for overlay-aware pool lookups. All other accessors
/// delegate to the base data.
pub struct MarketDataView<'a> {
    guard: tokio::sync::RwLockReadGuard<'a, MarketState>,
    overlay: Option<(StateLabel, OverlayStates)>,
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
    pub fn extract_subset_with_overlay(&self, component_ids: &HashSet<ComponentId>) -> MarketState {
        let mut subset = self.guard.extract_subset(component_ids);
        if let Some((_, ref states)) = self.overlay {
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
        }
        subset
    }

    /// Returns the component topology from the base data.
    pub fn component_topology(&self) -> HashMap<ComponentId, Vec<Address>> {
        self.guard.component_topology()
    }

    /// Extracts a base-data subset for the given component IDs (no overlay applied).
    pub fn extract_subset(&self, component_ids: &HashSet<ComponentId>) -> MarketState {
        self.guard.extract_subset(component_ids)
    }

    /// Returns a reference to the token registry from the base data.
    pub fn token_registry_ref(&self) -> &HashMap<Address, Token> {
        self.guard.token_registry_ref()
    }

    /// Returns the current gas price from the base data.
    pub fn gas_price(&self) -> Option<&BlockGasPrice> {
        self.guard.gas_price()
    }

    /// Returns the block info for the last base-state update.
    pub fn last_updated(&self) -> Option<&BlockInfo> {
        self.guard.last_updated()
    }

    /// Returns a token by address from the base data.
    pub fn get_token(&self, address: &Address) -> Option<&Token> {
        self.guard.get_token(address)
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
    /// All components indexed by their ID.
    components: HashMap<ComponentId, ProtocolComponent>,
    /// All states indexed by their component ID.
    pub(crate) simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
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

impl MarketState {
    /// Creates a new empty MarketState.
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

    /// Creates a filtered subset containing only data needed for the given components.
    ///
    /// This is used to create a local snapshot of market data that can be used for
    /// simulation without holding the main lock. The subset includes:
    /// - Components matching the provided IDs
    /// - Simulation states for those components (cloned via `clone_box`)
    /// - Tokens referenced by those components
    /// - Gas price and block info
    pub fn extract_subset(&self, component_ids: &HashSet<ComponentId>) -> MarketState {
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

        MarketState {
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
        let mut market = MarketState::new();

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

    // ==================== MarketData overlay tests ====================

    #[tokio::test]
    async fn register_and_retrieve_overlay_via_labeled_read() {
        let market_ref = MarketData::new_shared();

        let label = "test_label".to_string();
        let mut states: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        states.insert(
            "pool_ab".to_string(),
            Box::new(MockProtocolSim::new(99.0)) as Box<dyn ProtocolSim>,
        );

        market_ref
            .register_labeled_state(label.clone(), states, u64::MAX)
            .await;

        let guard = market_ref.read(Some(&label)).await;
        // Base data is empty — overlay provides the state
        let sim = guard.get_simulation_state("pool_ab");
        assert!(sim.is_some());
    }

    #[tokio::test]
    async fn read_without_label_returns_no_overlay() {
        let market_ref = MarketData::new_shared();

        market_ref
            .register_labeled_state(
                "my_label".to_string(),
                HashMap::from([(
                    "pool1".to_string(),
                    Box::new(MockProtocolSim::new(5.0)) as Box<dyn ProtocolSim>,
                )]),
                u64::MAX,
            )
            .await;

        // A handle with no label must not see the overlay
        let guard = market_ref.read(None).await;
        assert!(guard
            .get_simulation_state("pool1")
            .is_none());
    }

    #[tokio::test]
    async fn remove_labeled_state_clears_overlay() {
        let market_ref = MarketData::new_shared();
        let label = "lbl".to_string();

        market_ref
            .register_labeled_state(
                label.clone(),
                HashMap::from([(
                    "pool".to_string(),
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
                    HashMap::from([(
                        format!("pool_{i}"),
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
            HashMap::from([(
                "pool_x".to_string(),
                Box::new(MockProtocolSim::new(7.0)) as Box<dyn ProtocolSim>,
            )]),
            u64::MAX,
        )
        .await;

        let label = "shared".to_string();
        let guard_a = clone_a.read(Some(&label)).await;
        assert!(guard_a
            .get_simulation_state("pool_x")
            .is_some());
        drop(guard_a);

        let guard_b = clone_b.read(Some(&label)).await;
        assert!(guard_b
            .get_simulation_state("pool_x")
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
            data.upsert_components([mk_component("pool_ab", &[tok_a.clone(), tok_b.clone()])]);
            data.upsert_tokens([tok_a.clone(), tok_b.clone()]);
            data.update_states([(
                "pool_ab".to_string(),
                Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
            )]);
        }

        let label = "overlay".to_string();
        market_ref
            .register_labeled_state(
                label.clone(),
                HashMap::from([(
                    "pool_ab".to_string(),
                    Box::new(MockProtocolSim::new(99.0)) as Box<dyn ProtocolSim>,
                )]),
                u64::MAX,
            )
            .await;

        let guard = market_ref.read(Some(&label)).await;
        let ids: HashSet<ComponentId> = ["pool_ab".to_string()]
            .into_iter()
            .collect();
        let subset = guard.extract_subset_with_overlay(&ids);

        let sim = subset
            .get_simulation_state("pool_ab")
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
                HashMap::from([(
                    "pool_stale".to_string(),
                    Box::new(MockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
                )]),
                10,
            )
            .await;
        market_ref
            .register_labeled_state(
                "fresh".to_string(),
                HashMap::from([(
                    "pool_fresh".to_string(),
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

        let guard = market_ref.read(None).await;
        assert_eq!(
            guard
                .last_updated()
                .expect("last_updated must be set")
                .number(),
            1
        );
    }
}
