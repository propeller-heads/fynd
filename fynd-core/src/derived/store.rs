//! Typed storage for derived data.

use std::{any::Any, str::FromStr, sync::Arc};

use rustc_hash::FxHashMap;
use tokio::sync::RwLock;
use tycho_simulation::tycho_common::models::Address;

use super::{
    computation::{ComputationId, DerivedComputation, FailedItem, FailedItemError},
    computations::{ComponentDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
    types::{
        ComponentDepthKey, ComponentDepths, SpotPriceKey, SpotPrices, TokenGasPriceKey,
        TokenGasPrices, TokenPricesWithDeps,
    },
};
use crate::derived::SharedDerivedDataRef;

/// A computed value paired with the block it was computed for.
#[derive(Debug)]
struct ComputedValue<T> {
    data: T,
    block: u64,
}

/// A type-erased computation output paired with the block it was computed for.
struct ComputedSlot {
    data: Box<dyn Any + Send + Sync>,
    block: u64,
}

impl std::fmt::Debug for ComputedSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputedSlot")
            .field("block", &self.block)
            .finish_non_exhaustive()
    }
}

/// Typed storage for derived data computations.
///
/// Computation outputs live in a type-keyed slot map written erased and read back
/// typed by the per-computation getters below. The persistent failure maps stay
/// typed because their merge logic is specific to each keyed output.
#[derive(Debug, Default)]
pub struct DerivedData {
    /// Computation outputs keyed by [`ComputationId`], stored type-erased.
    slots: FxHashMap<ComputationId, ComputedSlot>,
    /// Persistent failure map: key → (block, error). Merged on incremental runs, replaced on full.
    token_prices_failed: FxHashMap<TokenGasPriceKey, (u64, FailedItemError)>,
    /// Token prices with path dependency tracking for incremental computation.
    token_prices_deps: Option<ComputedValue<TokenPricesWithDeps>>,
    /// Persistent failure map: key → (block, error). Merged on incremental runs, replaced on full.
    component_depths_failed: FxHashMap<ComponentDepthKey, (u64, FailedItemError)>,
    /// Persistent failure map: key → (block, error). Merged on incremental runs, replaced on full.
    spot_prices_failed: FxHashMap<SpotPriceKey, (u64, FailedItemError)>,
}

/// Parses `"component_id/token_in/token_out"` into a typed `(ComponentId, Address, Address)` key.
fn parse_pair_key(s: &str) -> Option<(String, Address, Address)> {
    let mut parts = s.rsplitn(3, '/');
    let token_out_str = parts.next()?;
    let token_in_str = parts.next()?;
    let component_id = parts.next()?;
    let token_in = Address::from_str(token_in_str).ok()?;
    let token_out = Address::from_str(token_out_str).ok()?;
    Some((component_id.to_string(), token_in, token_out))
}

impl DerivedData {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new shared derived data store for async computation tests that is wrapped in an
    /// `Arc<RwLock<>>`.
    pub fn new_shared() -> SharedDerivedDataRef {
        Arc::new(RwLock::new(Self::new()))
    }

    /// Stores a computation's output under its id, type-erased, replacing any prior value.
    pub(crate) fn set_output<T: Any + Send + Sync>(
        &mut self,
        id: ComputationId,
        data: T,
        block: u64,
    ) {
        self.slots
            .insert(id, ComputedSlot { data: Box::new(data), block });
    }

    /// Returns the output stored under `id` downcast to `T`, or `None` if absent.
    ///
    /// Each id maps to a single output type, so reading an existing slot as the wrong `T`
    /// is a programmer error: it trips a debug assertion and otherwise returns `None`.
    pub(crate) fn output<T: Any>(&self, id: ComputationId) -> Option<&T> {
        let slot = self.slots.get(id)?;
        debug_assert!(slot.data.is::<T>(), "derived output {id} read as the wrong type");
        slot.data.downcast_ref::<T>()
    }

    /// Returns the block at which the output under `id` was last computed.
    pub(crate) fn output_block(&self, id: ComputationId) -> Option<u64> {
        self.slots
            .get(id)
            .map(|slot| slot.block)
    }

    /// Removes the output stored under `id`.
    fn clear_output(&mut self, id: ComputationId) {
        self.slots.remove(id);
    }

    /// Returns `true` if all derived data types has been computed at least once.
    pub fn derived_data_ready(&self) -> bool {
        self.token_prices_block().is_some() &&
            self.token_prices_deps_block().is_some() &&
            self.component_depths_block().is_some() &&
            self.spot_prices_block().is_some()
    }

    // -------------------------------------------------------------------------
    // Token Prices
    // -------------------------------------------------------------------------

    /// Returns token prices if computed.
    pub fn token_prices(&self) -> Option<&TokenGasPrices> {
        self.token_prices_slot()
            .map(Arc::as_ref)
    }

    /// Returns token prices as a shared handle, if computed.
    ///
    /// For readers that outlive the lock on this store: cloning the handle costs a refcount
    /// rather than a copy of every token's price, which is what a solve would otherwise pay per
    /// order.
    pub fn token_prices_shared(&self) -> Option<Arc<TokenGasPrices>> {
        self.token_prices_slot().cloned()
    }

    fn token_prices_slot(&self) -> Option<&Arc<TokenGasPrices>> {
        self.output(TokenGasPriceComputation::ID)
    }

    /// Returns the block at which token prices were last computed.
    pub fn token_prices_block(&self) -> Option<u64> {
        self.output_block(TokenGasPriceComputation::ID)
    }

    /// Sets token prices, merging failures for incremental runs.
    ///
    /// For full recomputes, the failure map is replaced entirely. For incremental runs,
    /// failures are merged: existing entries for keys that now succeed are removed, new
    /// failures are inserted, and entries for keys not attempted this run are preserved.
    pub fn set_token_prices(
        &mut self,
        prices: TokenGasPrices,
        failed_items: Vec<FailedItem>,
        block: u64,
        is_full_recompute: bool,
    ) {
        let new_failures: FxHashMap<TokenGasPriceKey, (u64, FailedItemError)> = failed_items
            .into_iter()
            .filter_map(|f| {
                Address::from_str(&f.key)
                    .ok()
                    .map(|k| (k, (block, f.error)))
            })
            .collect();

        if is_full_recompute {
            self.token_prices_failed = new_failures;
        } else {
            self.token_prices_failed
                .retain(|k, _| !prices.contains_key(k));
            self.token_prices_failed
                .extend(new_failures);
        }

        self.set_output(TokenGasPriceComputation::ID, Arc::new(prices), block);
    }

    /// Returns `(block, error)` for this token address if it failed in a past
    /// computation, or `None` if it succeeded or was not attempted.
    pub fn token_price_failure(&self, key: &TokenGasPriceKey) -> Option<(u64, &FailedItemError)> {
        self.token_prices_failed
            .get(key)
            .map(|(block, error)| (*block, error))
    }

    /// Clears token prices and their failure map.
    pub fn clear_token_prices(&mut self) {
        self.clear_output(TokenGasPriceComputation::ID);
        self.token_prices_failed.clear();
    }

    // -------------------------------------------------------------------------
    // Token Prices with Dependencies (for incremental computation)
    // -------------------------------------------------------------------------

    /// Returns token prices with path dependencies if computed.
    pub fn token_prices_deps(&self) -> Option<&TokenPricesWithDeps> {
        self.token_prices_deps
            .as_ref()
            .map(|v| &v.data)
    }

    /// Returns the block at which token prices with dependencies were last computed.
    pub fn token_prices_deps_block(&self) -> Option<u64> {
        self.token_prices_deps
            .as_ref()
            .map(|v| v.block)
    }

    /// Sets token prices with path dependencies.
    pub fn set_token_prices_deps(&mut self, prices: TokenPricesWithDeps, block: u64) {
        self.token_prices_deps = Some(ComputedValue { data: prices, block });
    }

    /// Clears token prices with dependencies.
    pub fn clear_token_prices_deps(&mut self) {
        self.token_prices_deps = None;
    }

    // -------------------------------------------------------------------------
    // Component Depths
    // -------------------------------------------------------------------------

    /// Returns component depths if computed.
    pub fn component_depths(&self) -> Option<&ComponentDepths> {
        self.output(ComponentDepthComputation::ID)
    }

    /// Returns the block at which component depths were last computed.
    pub fn component_depths_block(&self) -> Option<u64> {
        self.output_block(ComponentDepthComputation::ID)
    }

    /// Sets component depths, merging failures for incremental runs.
    ///
    /// For full recomputes, the failure map is replaced entirely. For incremental runs,
    /// failures are merged: existing entries for keys that now succeed are removed, new
    /// failures are inserted, and entries for keys not attempted this run are preserved.
    pub fn set_component_depths(
        &mut self,
        depths: ComponentDepths,
        failed_items: Vec<FailedItem>,
        block: u64,
        is_full_recompute: bool,
    ) {
        let new_failures: FxHashMap<ComponentDepthKey, (u64, FailedItemError)> = failed_items
            .into_iter()
            .filter_map(|f| parse_pair_key(&f.key).map(|k| (k, (block, f.error))))
            .collect();

        if is_full_recompute {
            self.component_depths_failed = new_failures;
        } else {
            self.component_depths_failed
                .retain(|k, _| !depths.contains_key(k));
            self.component_depths_failed
                .extend(new_failures);
        }

        self.set_output(ComponentDepthComputation::ID, depths, block);
    }

    /// Returns `(block, error)` for this key if it failed in a past component depth
    /// computation, or `None` if it succeeded or was not attempted.
    ///
    /// Key format: `(component_id, token_in, token_out)`
    pub fn component_depth_failure(
        &self,
        key: &ComponentDepthKey,
    ) -> Option<(u64, &FailedItemError)> {
        self.component_depths_failed
            .get(key)
            .map(|(block, error)| (*block, error))
    }

    /// Clears component depths and their failure map.
    pub fn clear_component_depths(&mut self) {
        self.clear_output(ComponentDepthComputation::ID);
        self.component_depths_failed.clear();
    }

    // -------------------------------------------------------------------------
    // Spot Prices
    // -------------------------------------------------------------------------

    /// Returns spot prices if computed.
    pub fn spot_prices(&self) -> Option<&SpotPrices> {
        self.output(SpotPriceComputation::ID)
    }

    /// Returns the block at which spot prices were last computed.
    pub fn spot_prices_block(&self) -> Option<u64> {
        self.output_block(SpotPriceComputation::ID)
    }

    /// Sets spot prices, merging failures for incremental runs.
    ///
    /// For full recomputes, the failure map is replaced entirely. For incremental runs,
    /// failures are merged: existing entries for keys that now succeed are removed, new
    /// failures are inserted, and entries for keys not attempted this run are preserved.
    pub fn set_spot_prices(
        &mut self,
        prices: SpotPrices,
        failed_items: Vec<FailedItem>,
        block: u64,
        is_full_recompute: bool,
    ) {
        let new_failures: FxHashMap<SpotPriceKey, (u64, FailedItemError)> = failed_items
            .into_iter()
            .filter_map(|f| parse_pair_key(&f.key).map(|k| (k, (block, f.error))))
            .collect();

        if is_full_recompute {
            self.spot_prices_failed = new_failures;
        } else {
            self.spot_prices_failed
                .retain(|k, _| !prices.contains_key(k));
            self.spot_prices_failed
                .extend(new_failures);
        }

        self.set_output(SpotPriceComputation::ID, prices, block);
    }

    /// Returns `(block, error)` for this key if it failed in a past spot price
    /// computation, or `None` if it succeeded or was not attempted.
    ///
    /// Key format: `(component_id, token_in, token_out)`
    pub fn spot_price_failure(&self, key: &SpotPriceKey) -> Option<(u64, &FailedItemError)> {
        self.spot_prices_failed
            .get(key)
            .map(|(block, error)| (*block, error))
    }

    /// Clears spot prices and their failure map.
    pub fn clear_spot_prices(&mut self) {
        self.clear_output(SpotPriceComputation::ID);
        self.spot_prices_failed.clear();
    }

    // -------------------------------------------------------------------------
    // Bulk Operations
    // -------------------------------------------------------------------------

    /// Clears all stored data, including all failure maps.
    pub fn clear_all(&mut self) {
        self.slots.clear();
        self.token_prices_failed.clear();
        self.token_prices_deps = None;
        self.component_depths_failed.clear();
        self.spot_prices_failed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{algorithm::test_utils::addr, derived::types::SpotPrices};

    fn failed(key: &str, error: FailedItemError) -> FailedItem {
        FailedItem { key: key.to_string(), error }
    }

    fn pair_key(comp: &str, b_in: u8, b_out: u8) -> SpotPriceKey {
        (comp.to_string(), addr(b_in), addr(b_out))
    }

    #[test]
    fn test_token_prices_block_tracks_independently() {
        let mut store = DerivedData::new();
        assert_eq!(store.token_prices_block(), None);

        store.set_token_prices(Default::default(), vec![], 42, true);
        assert_eq!(store.token_prices_block(), Some(42));

        // Other computations not set yet
        assert_eq!(store.spot_prices_block(), None);
        assert_eq!(store.component_depths_block(), None);
    }

    #[test]
    fn test_spot_prices_block_tracks_independently() {
        let mut store = DerivedData::new();
        store.set_spot_prices(Default::default(), vec![], 10, true);
        assert_eq!(store.spot_prices_block(), Some(10));
        assert_eq!(store.token_prices_block(), None);
    }

    #[test]
    fn test_component_depths_block_tracks_independently() {
        let mut store = DerivedData::new();
        store.set_component_depths(Default::default(), vec![], 7, true);
        assert_eq!(store.component_depths_block(), Some(7));
        assert_eq!(store.token_prices_block(), None);
    }

    #[test]
    fn test_derived_data_ready() {
        let mut store = DerivedData::new();
        assert!(!store.derived_data_ready());

        store.set_spot_prices(Default::default(), vec![], 5, true);
        assert!(!store.derived_data_ready());

        store.set_token_prices(Default::default(), vec![], 10, true);
        assert!(!store.derived_data_ready());

        store.set_token_prices_deps(Default::default(), 10);
        assert!(!store.derived_data_ready());

        store.set_component_depths(Default::default(), vec![], 9, true);
        assert!(store.derived_data_ready());
    }

    #[test]
    fn test_clear_all_resets_all_fields() {
        let mut store = DerivedData::new();
        store.set_token_prices(Default::default(), vec![], 1, true);
        store.set_spot_prices(Default::default(), vec![], 1, true);
        store.set_component_depths(Default::default(), vec![], 1, true);

        store.clear_all();

        assert!(store.token_prices().is_none());
        assert!(store.spot_prices().is_none());
        assert!(store.component_depths().is_none());
        assert!(!store.derived_data_ready());
    }

    #[test]
    fn test_token_price_failure_stored_with_block() {
        let token_addr = addr(0xab);
        let key_str = format!("{token_addr}");
        let mut store = DerivedData::new();
        store.set_token_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::SimulationFailed("sim error".into()))],
            42,
            true,
        );
        assert_eq!(
            store.token_price_failure(&token_addr),
            Some((42, &FailedItemError::SimulationFailed("sim error".into())))
        );
        assert_eq!(store.token_price_failure(&addr(0xcd)), None);
    }

    #[test]
    fn test_spot_price_failure_stored_with_block() {
        let key = pair_key("component1", 0x01, 0x02);
        let key_str = format!("component1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_spot_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::SimulationFailed("sim error".into()))],
            10,
            true,
        );
        assert_eq!(
            store.spot_price_failure(&key),
            Some((10, &FailedItemError::SimulationFailed("sim error".into())))
        );
        assert_eq!(store.spot_price_failure(&pair_key("component1", 0x01, 0x03)), None);
    }

    #[test]
    fn test_component_depth_failure_stored_with_block() {
        let key: ComponentDepthKey = pair_key("component1", 0x01, 0x02);
        let key_str = format!("component1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_component_depths(
            Default::default(),
            vec![failed(&key_str, FailedItemError::SimulationFailed("depth error".into()))],
            7,
            true,
        );
        assert_eq!(
            store.component_depth_failure(&key),
            Some((7, &FailedItemError::SimulationFailed("depth error".into())))
        );
        assert_eq!(store.component_depth_failure(&pair_key("component2", 0x01, 0x02)), None);
    }

    #[test]
    fn test_rerunning_with_empty_failures_clears_old_reasons() {
        let key = pair_key("component1", 0x01, 0x02);
        let key_str = format!("component1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_spot_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::MissingSimulationState)],
            1,
            true,
        );
        assert!(store.spot_price_failure(&key).is_some());

        // Full re-run with no failures clears the map
        store.set_spot_prices(Default::default(), vec![], 2, true);
        assert_eq!(store.spot_price_failure(&key), None);
    }

    #[test]
    fn test_clear_token_prices_clears_failure_map() {
        let token_addr = addr(0xab);
        let key_str = format!("{token_addr}");
        let mut store = DerivedData::new();
        store.set_token_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::AllSimulationPathsFailed)],
            1,
            true,
        );
        store.clear_token_prices();
        assert_eq!(store.token_price_failure(&token_addr), None);
    }

    #[test]
    fn test_clear_spot_prices_clears_failure_map() {
        let key = pair_key("component1", 0x01, 0x02);
        let key_str = format!("component1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_spot_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::MissingSimulationState)],
            1,
            true,
        );
        store.clear_spot_prices();
        assert_eq!(store.spot_price_failure(&key), None);
    }

    #[test]
    fn test_clear_component_depths_clears_failure_map() {
        let key: ComponentDepthKey = pair_key("component1", 0x01, 0x02);
        let key_str = format!("component1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_component_depths(
            Default::default(),
            vec![failed(&key_str, FailedItemError::MissingSpotPrice)],
            1,
            true,
        );
        store.clear_component_depths();
        assert_eq!(store.component_depth_failure(&key), None);
    }

    #[test]
    fn test_incremental_run_preserves_failures_for_unattempted_items() {
        let key_a = pair_key("component_a", 0x01, 0x02);
        let key_a_str = format!("component_a/{}/{}", addr(0x01), addr(0x02));
        let key_b = pair_key("component_b", 0x03, 0x04);
        let key_b_str = format!("component_b/{}/{}", addr(0x03), addr(0x04));

        let mut store = DerivedData::new();

        // Full recompute at block 10: both keys fail
        store.set_spot_prices(
            Default::default(),
            vec![
                failed(&key_a_str, FailedItemError::MissingSimulationState),
                failed(&key_b_str, FailedItemError::MissingTokenMetadata),
            ],
            10,
            true,
        );
        assert_eq!(
            store.spot_price_failure(&key_a),
            Some((10, &FailedItemError::MissingSimulationState))
        );
        assert_eq!(
            store.spot_price_failure(&key_b),
            Some((10, &FailedItemError::MissingTokenMetadata))
        );

        // Incremental run at block 11: only component_b is attempted and succeeds
        let mut prices = SpotPrices::default();
        prices.insert(key_b.clone(), 1.0);
        store.set_spot_prices(prices, vec![], 11, false);

        // component_a was not attempted — failure is preserved from block 10
        assert_eq!(
            store.spot_price_failure(&key_a),
            Some((10, &FailedItemError::MissingSimulationState))
        );
        // component_b succeeded — failure is cleared
        assert_eq!(store.spot_price_failure(&key_b), None);
    }

    #[test]
    fn test_incremental_run_updates_block_on_repeated_failure() {
        let key = pair_key("component_a", 0x01, 0x02);
        let key_str = format!("component_a/{}/{}", addr(0x01), addr(0x02));

        let mut store = DerivedData::new();

        store.set_spot_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::MissingSimulationState)],
            10,
            true,
        );
        assert_eq!(
            store.spot_price_failure(&key),
            Some((10, &FailedItemError::MissingSimulationState))
        );

        // Incremental run at block 11: component_a fails again with a new error
        store.set_spot_prices(
            Default::default(),
            vec![failed(&key_str, FailedItemError::MissingTokenMetadata)],
            11,
            false,
        );
        assert_eq!(
            store.spot_price_failure(&key),
            Some((11, &FailedItemError::MissingTokenMetadata))
        );
    }

    #[test]
    fn test_clear_all_clears_all_failure_maps() {
        let token_addr = addr(0xab);
        let token_str = format!("{token_addr}");
        let pair = pair_key("component1", 0x01, 0x02);
        let pair_str = format!("component1/{}/{}", addr(0x01), addr(0x02));

        let mut store = DerivedData::new();
        store.set_token_prices(
            Default::default(),
            vec![failed(&token_str, FailedItemError::AllSimulationPathsFailed)],
            1,
            true,
        );
        store.set_spot_prices(
            Default::default(),
            vec![failed(&pair_str, FailedItemError::MissingSimulationState)],
            1,
            true,
        );
        store.set_component_depths(
            Default::default(),
            vec![failed(&pair_str, FailedItemError::MissingSpotPrice)],
            1,
            true,
        );

        store.clear_all();

        assert_eq!(store.token_price_failure(&token_addr), None);
        assert_eq!(store.spot_price_failure(&pair), None);
        assert_eq!(store.component_depth_failure(&pair), None);
    }
}
