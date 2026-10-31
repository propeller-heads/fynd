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

use std::{sync::Arc, time::Duration};

use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::RwLock;
use tracing::{info, warn};
use tycho_simulation::{
    tycho_client::feed::SynchronizerState,
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token, Address},
        simulation::protocol_sim::ProtocolSim,
    },
    tycho_ethereum::gas::BlockGasPrice,
};

use crate::{
    feed::component_filter::protocol_matches,
    types::{
        swap_directions, BlockInfo, ComponentId, ComponentTopology, ResolvedRouteConstraints,
        RouteConstraints, SwapDirection,
    },
};

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
}

impl MarketData {
    /// Creates a new handle wrapping the given data store.
    pub fn new(data: Arc<RwLock<MarketState>>) -> Self {
        Self { data, overlays: Arc::new(RwLock::new(FxHashMap::default())) }
    }

    /// Creates a new empty market data store wrapped in a `MarketData`.
    pub fn new_shared() -> Self {
        Self::new(Arc::new(RwLock::new(MarketState::new())))
    }

    /// Acquires a base view of the market data with no overlay applied.
    pub async fn read(&self) -> MarketDataView<'_> {
        MarketDataView { guard: self.data.read().await, overlay: None }
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
            return Ok(MarketDataView { guard, overlay: Some((label.clone(), states)) });
        }
        if &guard.label == label {
            return Ok(MarketDataView { guard, overlay: None });
        }
        Err(ReadLabeledError::NotFound(label.clone()))
    }

    /// Acquires an exclusive write guard on the base data store.
    pub async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, MarketState> {
        self.data.write().await
    }

    /// Extracts a base-data subset for `component_ids` without holding the read guard across the
    /// whole clone.
    ///
    /// `extract_subset` deep-clones one simulation state per component, so one guard held over a
    /// near-whole-market set stalls the feed's writer — and every reader queued behind it — for
    /// the whole clone. This clones in batches of 512 components, releasing the guard between
    /// batches. A batch whose label differs from the first batch's means the feed advanced
    /// mid-clone; the clone restarts so the returned snapshot stays single-block consistent.
    /// After three attempts it falls back to one guard for the full set: a feed advancing faster
    /// than the batched clone completes would otherwise starve it forever.
    ///
    /// No overlay is applied — this reads base data only.
    pub async fn extract_subset_batched(
        &self,
        component_ids: &FxHashSet<&ComponentId>,
    ) -> MarketState {
        self.extract_subset_in_batches(component_ids, 512, || {})
            .await
    }

    /// `extract_subset_batched` with the batch size injectable, plus a hook that runs after each
    /// batch — the window where no guard is held and a feed write can land mid-clone. Tests use
    /// the two to drive the merge, restart, and fallback paths deterministically.
    async fn extract_subset_in_batches(
        &self,
        component_ids: &FxHashSet<&ComponentId>,
        batch_size: usize,
        between_batches: impl Fn(),
    ) -> MarketState {
        if component_ids.is_empty() {
            // The chunk loop would yield a blank default; extract_subset carries the label,
            // block, and gas price even for an empty set.
            return self
                .data
                .read()
                .await
                .extract_subset(component_ids);
        }
        let ids: Vec<&ComponentId> = component_ids.iter().copied().collect();
        'attempt: for _ in 0..3 {
            let mut merged: Option<MarketState> = None;
            for chunk in ids.chunks(batch_size) {
                let chunk_ids: FxHashSet<&ComponentId> = chunk.iter().copied().collect();
                let part = self
                    .data
                    .read()
                    .await
                    .extract_subset(&chunk_ids);
                match &mut merged {
                    None => merged = Some(part),
                    Some(snapshot) => {
                        if part.label != snapshot.label {
                            continue 'attempt;
                        }
                        snapshot.merge_subset(part);
                    }
                }
                between_batches();
            }
            return merged.unwrap_or_default();
        }
        warn!(
            components = ids.len(),
            "batched market snapshot restarted three times; falling back to one guard over the \
             full set"
        );
        self.data
            .read()
            .await
            .extract_subset(component_ids)
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
    /// The overlay is not applied, so this only exposes the base market state. Returns `None`
    /// if the lock is currently held for writing. Callers must treat this as unavailable data,
    /// not as an error.
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

    /// Returns the swap directions of one component in the base data.
    pub(crate) fn swap_directions_of(&self, id: &str) -> Option<&[SwapDirection]> {
        self.guard.swap_directions_of(id)
    }

    /// Returns the tokens and swap directions of one component in the base data.
    pub(crate) fn topology_of(&self, id: &str) -> Option<ComponentTopology> {
        self.guard.topology_of(id)
    }

    /// Returns the component topology from the base data.
    pub fn component_topology(&self) -> FxHashMap<ComponentId, ComponentTopology> {
        self.guard.component_topology()
    }

    /// Extracts a base-data subset for the given component IDs (no overlay applied).
    pub fn extract_subset(&self, component_ids: &FxHashSet<&ComponentId>) -> MarketState {
        self.guard.extract_subset(component_ids)
    }

    /// Returns a reference to the token registry from the base data.
    pub fn token_registry_ref(&self) -> &FxHashMap<Address, Arc<Token>> {
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

/// A component with the swap directions computed when it was stored.
#[derive(Debug, Clone)]
struct StoredComponent {
    component: Arc<ProtocolComponent>,
    swap_directions: Arc<[SwapDirection]>,
}

/// What `MarketState::upsert_components` found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpsertedComponents {
    /// Ids the market did not hold before.
    pub new: Vec<ComponentId>,
    /// Ids the market held, stored again with other swap directions.
    pub reshaped: Vec<ComponentId>,
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
    /// All components indexed by their ID, each with its swap directions.
    components: FxHashMap<ComponentId, StoredComponent>,
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
    /// Protocol systems excluded from routing because they are out of sync; see
    /// [`MarketState::update_sync_status`].
    unsynced_protocols: FxHashSet<String>,
    /// The components of `unsynced_protocols`, shared with every request's
    /// [`ResolvedRouteConstraints`].
    unsynced_pools: Arc<FxHashSet<ComponentId>>,
    /// Pools the `fynd-nexus` service reports as unhealthy, excluded from routing like
    /// `unsynced_pools`; see [`MarketState::set_unhealthy_pools`]. Shared with every request's
    /// [`ResolvedRouteConstraints`].
    unhealthy_pools: Arc<FxHashSet<ComponentId>>,
    /// The components of each protocol system, maintained on upsert/remove so a quote that
    /// excludes a protocol names that system's pools without scanning the component map, and so
    /// the metrics sampler can count them without one either.
    components_by_protocol: FxHashMap<String, FxHashSet<ComponentId>>,
    /// Changes when a component is added or removed, `unsynced_protocols` changes, or
    /// `unhealthy_pools` changes: the things a resolved [`ResolvedRouteConstraints`] depends on.
    /// Not when a component's state changes.
    exclusion_generation: u64,
}

impl StoredComponent {
    fn topology(&self) -> ComponentTopology {
        ComponentTopology {
            tokens: self.component.tokens.clone(),
            swap_directions: Arc::clone(&self.swap_directions),
        }
    }
}

impl MarketState {
    /// The pools and tokens `filter` excludes and the tokens it requires, with each protocol
    /// system it names replaced by the components this market holds for that system.
    ///
    /// A protocol system matches exactly (`uniswap_v2`), or as a family when the entry ends in
    /// `:` (`fallback:`). An entry this market holds no component for excludes nothing.
    #[must_use]
    pub fn resolve_route_filter(&self, filter: &RouteConstraints) -> ResolvedRouteConstraints {
        let mut pools = filter.excluded_pools().clone();
        for entry in filter.excluded_protocols() {
            if entry.ends_with(':') {
                for (system, ids) in &self.components_by_protocol {
                    if protocol_matches(entry, system) {
                        pools.extend(ids.iter().cloned());
                    }
                }
            } else {
                pools.extend(
                    self.components_by_protocol(entry)
                        .cloned(),
                );
            }
        }
        ResolvedRouteConstraints {
            pools,
            tokens: filter.excluded_tokens().clone(),
            required_tokens: filter.required_tokens().clone(),
            unsynced_pools: Arc::clone(&self.unsynced_pools),
            unhealthy_pools: Arc::clone(&self.unhealthy_pools),
        }
    }

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
            unsynced_protocols: FxHashSet::default(),
            unsynced_pools: Arc::default(),
            unhealthy_pools: Arc::default(),
            components_by_protocol: FxHashMap::default(),
            exclusion_generation: 0,
        }
    }

    /// Returns the label identifying the block or overlay this state was produced from.
    pub fn label(&self) -> &StateLabel {
        &self.label
    }

    /// Returns the generation of what route exclusions depend on: the component set and the
    /// unsynced protocols.
    pub fn exclusion_generation(&self) -> u64 {
        self.exclusion_generation
    }

    /// Returns the protocol systems excluded from routing because they are out of sync.
    pub fn unsynced_protocols(&self) -> &FxHashSet<String> {
        &self.unsynced_protocols
    }

    /// Whether this component's protocol is out of sync and so excluded from routing.
    pub fn is_unsynced_pool(&self, component_id: &str) -> bool {
        self.unsynced_pools
            .contains(component_id)
    }

    /// Whether the `fynd-nexus` service reports this component as unhealthy, so routing skips
    /// it.
    pub fn is_unhealthy_pool(&self, component_id: &str) -> bool {
        self.unhealthy_pools
            .contains(component_id)
    }

    /// Returns the pools the `fynd-nexus` service reports as unhealthy.
    pub fn unhealthy_pools(&self) -> &FxHashSet<ComponentId> {
        &self.unhealthy_pools
    }

    /// Replaces the pools excluded from routing as unhealthy, and returns whether the set
    /// changed.
    ///
    /// The exclusion generation only moves when the set differs from the current one, so an
    /// unchanged report does not invalidate the exclusions requests have resolved. An id for a
    /// component the market does not hold is kept: the component may arrive later.
    pub fn set_unhealthy_pools(&mut self, pools: FxHashSet<ComponentId>) -> bool {
        if *self.unhealthy_pools == pools {
            return false;
        }
        self.unhealthy_pools = Arc::new(pools);
        self.exclusion_generation = self
            .exclusion_generation
            .wrapping_add(1);
        true
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

    /// Number of components per protocol system.
    ///
    /// Entries stay present at zero after all of a protocol's components are removed, so exported
    /// gauges reset instead of freezing at the last value.
    pub fn component_counts_by_protocol(&self) -> FxHashMap<String, u64> {
        self.components_by_protocol
            .iter()
            .map(|(protocol_system, ids)| (protocol_system.clone(), ids.len() as u64))
            .collect()
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

    /// Returns each component's tokens and swap directions. The directions are shared, not copied.
    pub fn component_topology(&self) -> FxHashMap<ComponentId, ComponentTopology> {
        self.components
            .iter()
            .map(|(id, stored)| (id.clone(), stored.topology()))
            .collect()
    }

    /// Returns the swap directions of one component.
    pub(crate) fn swap_directions_of(&self, id: &str) -> Option<&[SwapDirection]> {
        self.components
            .get(id)
            .map(|stored| &stored.swap_directions[..])
    }

    /// Returns the tokens and swap directions of one component.
    pub(crate) fn topology_of(&self, id: &str) -> Option<ComponentTopology> {
        self.components
            .get(id)
            .map(StoredComponent::topology)
    }

    /// Gets a component by ID.
    pub fn get_component(&self, id: &str) -> Option<&ProtocolComponent> {
        self.components
            .get(id)
            .map(|stored| stored.component.as_ref())
    }

    /// The ids of every component this protocol system holds. Empty for a system the market does
    /// not carry.
    pub fn components_by_protocol(
        &self,
        protocol_system: &str,
    ) -> impl Iterator<Item = &ComponentId> {
        self.components_by_protocol
            .get(protocol_system)
            .into_iter()
            .flatten()
    }

    /// Gets a component by ID as a shared handle, for callers that need to keep it.
    pub fn get_component_shared(&self, id: &str) -> Option<&Arc<ProtocolComponent>> {
        self.components
            .get(id)
            .map(|stored| &stored.component)
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

    /// Inserts or updates components. Reports the ids that are new and the ids stored again with
    /// other swap directions.
    pub fn upsert_components(
        &mut self,
        components: impl IntoIterator<Item = ProtocolComponent>,
    ) -> UpsertedComponents {
        let mut upserted = UpsertedComponents::default();
        for component in components {
            let protocol_system = component.protocol_system.clone();
            let component_id = component.id.clone();
            let swap_directions = swap_directions(&component);
            let is_new = match self.components.get(&component_id) {
                None => {
                    upserted.new.push(component_id.clone());
                    true
                }
                Some(stored) if stored.swap_directions != swap_directions => {
                    upserted
                        .reshaped
                        .push(component_id.clone());
                    false
                }
                Some(_) => false,
            };
            self.components.insert(
                component_id.clone(),
                StoredComponent { component: Arc::new(component), swap_directions },
            );
            if is_new &&
                self.unsynced_protocols
                    .contains(&protocol_system)
            {
                Arc::make_mut(&mut self.unsynced_pools).insert(component_id.clone());
            }
            self.components_by_protocol
                .entry(protocol_system)
                .or_default()
                .insert(component_id);
            if is_new {
                self.exclusion_generation = self
                    .exclusion_generation
                    .wrapping_add(1);
            }
        }
        upserted
    }

    /// Inserts or updates tokens.
    pub fn upsert_tokens(&mut self, tokens: impl IntoIterator<Item = Token>) {
        for token in tokens {
            self.tokens
                .insert(token.address.clone(), Arc::new(token));
        }
    }

    /// Applies the sync status Tycho reports for each protocol, then recomputes which protocols
    /// are out of sync. Does nothing for an update that carries no sync status, such as an RFQ or
    /// price level update.
    ///
    /// The newest `Ready` block in the update becomes the last updated block. A protocol is out
    /// of sync when Tycho reports it `Stale` or `Ended`, or `Delayed` with a block whose timestamp
    /// is more than `max_sync_lag` behind the last updated block's. Its components are
    /// excluded from routing until it is back in sync.
    pub fn update_sync_status(
        &mut self,
        sync_states: impl IntoIterator<Item = (String, SynchronizerState)>,
        max_sync_lag: Duration,
    ) {
        let mut has_status = false;
        let mut newest_ready: Option<BlockInfo> = None;
        for (protocol_system, status) in sync_states {
            has_status = true;
            if let SynchronizerState::Ready(header) = &status {
                if newest_ready
                    .as_ref()
                    .is_none_or(|newest| header.number > newest.number())
                {
                    newest_ready = Some(BlockInfo::new(
                        header.number,
                        header.hash.to_string(),
                        header.timestamp,
                    ));
                }
            }
            self.protocol_sync_status
                .insert(protocol_system, status);
        }
        if !has_status {
            return;
        }
        if let Some(block_info) = newest_ready {
            self.last_updated = Some(block_info);
        }
        self.refresh_unsynced_protocols(max_sync_lag);
    }

    /// Recomputes which protocol systems are out of sync and which of their components are
    /// excluded from routing.
    fn refresh_unsynced_protocols(&mut self, max_sync_lag: Duration) {
        let last_updated_timestamp = self
            .last_updated
            .as_ref()
            .map(BlockInfo::timestamp);
        let unsynced: FxHashSet<String> = self
            .protocol_sync_status
            .iter()
            .filter(|(_, status)| is_unsynced(status, last_updated_timestamp, max_sync_lag))
            .map(|(protocol_system, _)| protocol_system.clone())
            .collect();
        if unsynced == self.unsynced_protocols {
            return;
        }
        for protocol_system in unsynced.difference(&self.unsynced_protocols) {
            warn!(
                protocol_system,
                status = ?self.protocol_sync_status.get(protocol_system),
                last_updated_timestamp,
                pools = self
                    .components_by_protocol(protocol_system)
                    .count(),
                "protocol out of sync, excluding its pools from routing"
            );
        }
        for protocol_system in self
            .unsynced_protocols
            .difference(&unsynced)
        {
            info!(protocol_system, "protocol back in sync, routing through its pools again");
        }
        let mut unsynced_pools = FxHashSet::default();
        for protocol_system in &unsynced {
            unsynced_pools.extend(
                self.components_by_protocol(protocol_system)
                    .cloned(),
            );
        }
        self.unsynced_protocols = unsynced;
        self.unsynced_pools = Arc::new(unsynced_pools);
        self.exclusion_generation = self
            .exclusion_generation
            .wrapping_add(1);
    }

    /// Removes a component.
    pub fn remove_components<'a>(&mut self, ids: impl IntoIterator<Item = &'a ComponentId>) {
        for id in ids {
            if let Some(stored) = self.components.remove(id) {
                self.exclusion_generation = self
                    .exclusion_generation
                    .wrapping_add(1);
                if self.unsynced_pools.contains(id) {
                    Arc::make_mut(&mut self.unsynced_pools).remove(id);
                }
                if let Some(ids) = self
                    .components_by_protocol
                    .get_mut(&stored.component.protocol_system)
                {
                    ids.remove(id);
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

    /// Creates a filtered subset containing only data needed for the given components.
    ///
    /// This is used to create a local snapshot of market data that can be used for
    /// simulation without holding the main lock. The subset includes:
    /// - Components matching the provided IDs
    /// - Simulation states for those components (cloned via `clone_box`)
    /// - Tokens referenced by those components
    /// - Gas price and block info
    /// - The unsynced protocols and their pools
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
            if let Some(stored) = self.components.get(id) {
                token_addresses.extend(&stored.component.tokens);
                components.insert(id.clone(), stored.clone());
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

        let components_by_protocol = index_by_protocol(&components);

        MarketState {
            label: self.label.clone(),
            components,
            simulation_states,
            tokens,
            gas_price: self.gas_price.clone(),
            protocol_sync_status: FxHashMap::default(), // Not needed for simulation
            last_updated: self.last_updated.clone(),
            unsynced_protocols: self.unsynced_protocols.clone(),
            unsynced_pools: Arc::clone(&self.unsynced_pools),
            unhealthy_pools: Arc::clone(&self.unhealthy_pools),
            components_by_protocol,
            exclusion_generation: self.exclusion_generation,
        }
    }

    /// Absorbs another subset extracted from the same base state (the caller checks the labels
    /// match), keeping this one's metadata. Component sets from `extract_subset` batches are
    /// disjoint, so components and simulation states are never overwritten; a token shared by
    /// two batches is overwritten with an identical clone from the same base state.
    fn merge_subset(&mut self, other: MarketState) {
        self.components.extend(other.components);
        self.simulation_states
            .extend(other.simulation_states);
        self.tokens.extend(other.tokens);
    }
}

/// Whether `status` excludes a protocol from routing. See [`MarketState::update_sync_status`].
fn is_unsynced(
    status: &SynchronizerState,
    last_updated_timestamp: Option<u64>,
    max_sync_lag: Duration,
) -> bool {
    match status {
        SynchronizerState::Started |
        SynchronizerState::Ready(_) |
        SynchronizerState::Advanced(_) => false,
        SynchronizerState::Stale(_) | SynchronizerState::Ended(_) => true,
        SynchronizerState::Delayed(header) => last_updated_timestamp.is_some_and(|last_updated| {
            Duration::from_secs(last_updated.saturating_sub(header.timestamp)) > max_sync_lag
        }),
    }
}

/// Groups component ids by the protocol system their component carries.
fn index_by_protocol(
    components: &FxHashMap<ComponentId, StoredComponent>,
) -> FxHashMap<String, FxHashSet<ComponentId>> {
    let mut index: FxHashMap<String, FxHashSet<ComponentId>> = FxHashMap::default();
    for (id, stored) in components {
        index
            .entry(stored.component.protocol_system.clone())
            .or_default()
            .insert(id.clone());
    }
    index
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::{tycho_client::feed::BlockHeader, tycho_ethereum::gas::GasPrice};

    use super::*;
    use crate::algorithm::test_utils::{
        component, component_with_directions, component_with_protocol, set_last_updated_block,
        token, MockProtocolSim,
    };

    #[test]
    fn test_upsert_components_reports_new_and_reshaped_ids() {
        let (a, b, c) = (token(0x01, "A"), token(0x02, "B"), token(0x03, "C"));
        let tokens = [a.clone(), b.clone(), c.clone()];
        let venue = |directions: &[(&Token, &Token)]| {
            component_with_directions("venue", &tokens, directions)
        };
        let mut market = MarketState::new();
        let id = || vec!["venue".to_string()];

        let new = market.upsert_components([venue(&[(&a, &b)])]);
        let unchanged = market.upsert_components([venue(&[(&a, &b)])]);
        let reshaped = market.upsert_components([venue(&[(&b, &c)])]);

        assert_eq!(new, UpsertedComponents { new: id(), reshaped: Vec::new() });
        assert_eq!(unchanged, UpsertedComponents::default());
        assert_eq!(reshaped, UpsertedComponents { new: Vec::new(), reshaped: id() });
    }

    #[test]
    fn test_resolve_route_filter_with_protocol_prefix() {
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let mut market = MarketState::new();
        market.upsert_components([
            component_with_protocol("pamm", "fallback:fermiswap", &[a.clone(), b.clone()]),
            component_with_protocol("v3", "uniswap_v3", &[a, b]),
        ]);
        let prefix = market.resolve_route_filter(
            &RouteConstraints::default().with_excluded_protocols(["fallback:".to_string()]),
        );
        let partial = market.resolve_route_filter(
            &RouteConstraints::default().with_excluded_protocols(["propamm".to_string()]),
        );
        assert!(prefix.excludes_pool("pamm"));
        assert!(!prefix.excludes_pool("v3"));
        assert!(partial.is_empty());
    }

    /// A filter names protocol systems; a solve reads pools, so resolving replaces each system
    /// with that system's pools and leaves every other pool alone.
    #[test]
    fn test_resolve_route_filter_with_a_protocol() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let mut market = MarketState::new();
        market.upsert_components([
            component_with_protocol("v2_pool", "uniswap_v2", &[token_a.clone(), token_b.clone()]),
            component_with_protocol("v3_pool", "uniswap_v3", &[token_a.clone(), token_b.clone()]),
        ]);

        let filter = RouteConstraints::default()
            .with_excluded_pools(["named_pool".to_string()])
            .with_excluded_protocols(["uniswap_v2".to_string()])
            .with_excluded_tokens([token_b.address.clone()]);
        let exclusions = market.resolve_route_filter(&filter);

        assert!(exclusions.excludes_pool("v2_pool"), "the protocol's own pool is excluded");
        assert!(exclusions.excludes_pool("named_pool"), "a pool named directly stays excluded");
        assert!(!exclusions.excludes_pool("v3_pool"), "another protocol's pool is untouched");
        assert!(exclusions.excludes_token(&token_b.address));
        assert!(
            market
                .resolve_route_filter(
                    &RouteConstraints::default()
                        .with_excluded_protocols(["not_a_protocol".to_string()])
                )
                .is_empty(),
            "a system the market holds no pool of excludes nothing"
        );
    }

    #[test]
    fn test_resolve_route_filter_with_required_tokens() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let market = MarketState::new();

        let exclusions = market.resolve_route_filter(
            &RouteConstraints::default().with_required_tokens([token_b.address.clone()]),
        );

        assert!(exclusions.visits_all_required_tokens([&token_b.address]));
        assert!(!exclusions.visits_all_required_tokens([&token_a.address]));
    }

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
        set_last_updated_block(&mut market, 12345, 0);

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

    const MAX_SYNC_LAG: Duration = Duration::from_secs(15);

    /// A header on a chain with 12-second blocks.
    fn header(number: u64) -> BlockHeader {
        BlockHeader { number, timestamp: number * 12, ..Default::default() }
    }

    /// A market at block 100 with a `uniswap_v2` pool reporting `Ready` and a `uniswap_v3` pool
    /// reporting `v3_status`, updated with `MAX_SYNC_LAG`.
    fn market_with_v3_status(v3_status: SynchronizerState) -> MarketState {
        let a = token(0x01, "A");
        let b = token(0x02, "B");
        let mut market = MarketState::new();
        market.upsert_components([
            component_with_protocol("v2_pool", "uniswap_v2", &[a.clone(), b.clone()]),
            component_with_protocol("v3_pool", "uniswap_v3", &[a, b]),
        ]);
        market.update_sync_status(
            [
                ("uniswap_v2".to_string(), SynchronizerState::Ready(header(100))),
                ("uniswap_v3".to_string(), v3_status),
            ],
            MAX_SYNC_LAG,
        );
        market
    }

    #[rstest::rstest]
    #[case::ready(SynchronizerState::Ready(header(100)), false)]
    #[case::started(SynchronizerState::Started, false)]
    #[case::advanced(SynchronizerState::Advanced(header(101)), false)]
    #[case::delayed_within_max_lag(SynchronizerState::Delayed(header(99)), false)]
    #[case::delayed_beyond_max_lag(SynchronizerState::Delayed(header(98)), true)]
    #[case::stale(SynchronizerState::Stale(header(99)), true)]
    #[case::ended(SynchronizerState::Ended("closed".to_string()), true)]
    fn test_update_sync_status(#[case] status: SynchronizerState, #[case] excluded: bool) {
        let market = market_with_v3_status(status);

        let exclusions = market.resolve_route_filter(&RouteConstraints::default());
        assert_eq!(exclusions.excludes_pool("v3_pool"), excluded);
        assert_eq!(
            market
                .unsynced_protocols()
                .contains("uniswap_v3"),
            excluded
        );
        assert!(!exclusions.excludes_pool("v2_pool"));
    }

    /// RFQ and price level updates carry no sync status.
    #[test]
    fn test_update_sync_status_without_states() {
        let mut market = market_with_v3_status(SynchronizerState::Stale(header(90)));
        let generation = market.exclusion_generation();

        market.update_sync_status([], MAX_SYNC_LAG);

        assert!(market.is_unsynced_pool("v3_pool"));
        assert_eq!(market.exclusion_generation(), generation);
    }

    #[test]
    fn test_unsynced_protocol_recovery() {
        let mut market = market_with_v3_status(SynchronizerState::Stale(header(90)));
        let generation = market.exclusion_generation();

        market.update_sync_status(
            [("uniswap_v3".to_string(), SynchronizerState::Ready(header(100)))],
            MAX_SYNC_LAG,
        );

        assert_ne!(market.exclusion_generation(), generation);
        assert!(market
            .resolve_route_filter(&RouteConstraints::default())
            .is_empty());
    }

    /// A stuck protocol keeps reporting the same `Delayed` header while the chain moves on.
    #[test]
    fn test_delayed_protocol_falling_behind() {
        let mut market = market_with_v3_status(SynchronizerState::Delayed(header(99)));

        market.update_sync_status(
            [("uniswap_v2".to_string(), SynchronizerState::Ready(header(102)))],
            MAX_SYNC_LAG,
        );

        assert!(market.is_unsynced_pool("v3_pool"));
    }

    #[test]
    fn test_unsynced_pools_new_components() {
        let mut market = market_with_v3_status(SynchronizerState::Stale(header(90)));
        let tokens = [token(0x01, "A"), token(0x03, "C")];

        market.upsert_components([
            component_with_protocol("new_v3_pool", "uniswap_v3", &tokens),
            component_with_protocol("new_v2_pool", "uniswap_v2", &tokens),
        ]);

        assert!(market.is_unsynced_pool("new_v3_pool"));
        assert!(!market.is_unsynced_pool("new_v2_pool"));
    }

    #[test]
    fn test_unsynced_pools_removed_component() {
        let mut market = market_with_v3_status(SynchronizerState::Stale(header(90)));

        market.remove_components(["v3_pool".to_string()].iter());

        assert!(!market.is_unsynced_pool("v3_pool"));
    }

    fn pool_set(ids: &[&str]) -> FxHashSet<ComponentId> {
        ids.iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn test_set_unhealthy_pools_excludes_from_routing() {
        let mut market = market_with_v3_status(SynchronizerState::Ready(header(100)));

        let changed = market.set_unhealthy_pools(pool_set(&["v3_pool"]));

        let exclusions = market.resolve_route_filter(&RouteConstraints::default());
        assert!(changed);
        assert!(exclusions.excludes_pool("v3_pool"));
        assert!(!exclusions.excludes_pool("v2_pool"));
        assert!(market.is_unhealthy_pool("v3_pool"));
        assert!(!exclusions.is_empty());
    }

    #[test]
    fn test_set_unhealthy_pools_moves_generation_only_on_change() {
        let mut market = market_with_v3_status(SynchronizerState::Ready(header(100)));
        market.set_unhealthy_pools(pool_set(&["v3_pool"]));
        let generation = market.exclusion_generation();

        let unchanged = market.set_unhealthy_pools(pool_set(&["v3_pool"]));
        assert!(!unchanged);
        assert_eq!(market.exclusion_generation(), generation);

        let cleared = market.set_unhealthy_pools(FxHashSet::default());
        assert!(cleared);
        assert_ne!(market.exclusion_generation(), generation);
        assert!(market
            .resolve_route_filter(&RouteConstraints::default())
            .is_empty());
    }

    /// The component may arrive after the report that names it.
    #[test]
    fn test_set_unhealthy_pools_keeps_unknown_ids() {
        let mut market = MarketState::new();
        market.set_unhealthy_pools(pool_set(&["not_yet_here"]));

        assert!(market.is_unhealthy_pool("not_yet_here"));
    }

    fn market_with_two_components() -> MarketState {
        let mut market = MarketState::new();
        let token_a = token(0x0A, "A");
        let token_b = token(0x0B, "B");
        let token_c = token(0x0C, "C");
        market.upsert_components([
            component("component_ab", &[token_a.clone(), token_b.clone()]),
            component("component_bc", &[token_b.clone(), token_c.clone()]),
        ]);
        market.upsert_tokens([token_a, token_b, token_c]);
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
        set_last_updated_block(&mut market, 12345, 0);
        market
    }

    #[test]
    fn test_merge_subset() {
        let market = market_with_two_components();
        let ab = "component_ab".to_string();
        let bc = "component_bc".to_string();

        let mut merged = market.extract_subset(&[&ab].into_iter().collect());
        merged.merge_subset(market.extract_subset(&[&bc].into_iter().collect()));

        let combined = market.extract_subset(&[&ab, &bc].into_iter().collect());
        assert_eq!(merged.components.len(), combined.components.len());
        assert_eq!(merged.simulation_states.len(), combined.simulation_states.len());
        assert_eq!(merged.tokens.len(), combined.tokens.len());
        assert_eq!(merged.label, combined.label);
    }

    #[tokio::test]
    async fn test_extract_subset_batched() {
        let market_data = MarketData::new(Arc::new(RwLock::new(market_with_two_components())));
        let ab = "component_ab".to_string();
        let bc = "component_bc".to_string();
        let ids: FxHashSet<&ComponentId> = [&ab, &bc].into_iter().collect();

        let batched = market_data
            .extract_subset_batched(&ids)
            .await;

        let direct = market_data
            .read()
            .await
            .extract_subset(&ids);
        assert_eq!(batched.components.len(), direct.components.len());
        assert_eq!(batched.simulation_states.len(), direct.simulation_states.len());
        assert_eq!(batched.tokens.len(), direct.tokens.len());
        assert_eq!(batched.label, direct.label);
    }

    #[tokio::test]
    async fn test_extract_subset_batched_multi_chunk() {
        let market_data = MarketData::new(Arc::new(RwLock::new(market_with_two_components())));
        let ab = "component_ab".to_string();
        let bc = "component_bc".to_string();
        let ids: FxHashSet<&ComponentId> = [&ab, &bc].into_iter().collect();

        // Batch size 1 puts each component in its own chunk, so the merge path runs.
        let batched = market_data
            .extract_subset_in_batches(&ids, 1, || {})
            .await;

        let direct = market_data
            .read()
            .await
            .extract_subset(&ids);
        assert!(batched.components.contains_key(&ab));
        assert!(batched.components.contains_key(&bc));
        assert!(batched
            .simulation_states
            .contains_key(&ab));
        assert!(batched
            .simulation_states
            .contains_key(&bc));
        assert_eq!(batched.tokens.len(), direct.tokens.len());
        assert_eq!(batched.label, direct.label);
    }

    #[tokio::test]
    async fn test_extract_subset_batched_label_change_mid_clone() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let market_data = MarketData::new(Arc::new(RwLock::new(market_with_two_components())));
        let ab = "component_ab".to_string();
        let bc = "component_bc".to_string();
        let ids: FxHashSet<&ComponentId> = [&ab, &bc].into_iter().collect();
        // The feed advances once, after the first batch of the first attempt: the second
        // batch's label mismatch must restart the clone, and the second attempt must read one
        // consistent post-advance state.
        let batches_done = AtomicUsize::new(0);
        let writer = market_data.clone();

        let batched = market_data
            .extract_subset_in_batches(&ids, 1, || {
                if batches_done.fetch_add(1, Ordering::SeqCst) == 0 {
                    writer
                        .try_write()
                        .expect("no guard is held between batches")
                        .label = "advanced".to_string();
                }
            })
            .await;

        assert_eq!(batched.label, "advanced");
        assert!(batched.components.contains_key(&ab));
        assert!(batched.components.contains_key(&bc));
    }

    #[tokio::test]
    async fn test_extract_subset_batched_fallback_after_three_attempts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let market_data = MarketData::new(Arc::new(RwLock::new(market_with_two_components())));
        let ab = "component_ab".to_string();
        let bc = "component_bc".to_string();
        let ids: FxHashSet<&ComponentId> = [&ab, &bc].into_iter().collect();
        // The feed advances after every batch, so all three batched attempts restart and the
        // full-set fallback must still deliver every component under one guard.
        let bumps = AtomicUsize::new(0);
        let writer = market_data.clone();

        let batched = market_data
            .extract_subset_in_batches(&ids, 1, || {
                let bump = bumps.fetch_add(1, Ordering::SeqCst);
                writer
                    .try_write()
                    .expect("no guard is held between batches")
                    .label = format!("block_{bump}");
            })
            .await;

        assert!(batched.components.contains_key(&ab));
        assert!(batched.components.contains_key(&bc));
        assert!(batched
            .simulation_states
            .contains_key(&ab));
        assert!(batched
            .simulation_states
            .contains_key(&bc));
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
                set_last_updated_block(data, 1, 0);
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
}
