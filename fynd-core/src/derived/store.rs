//! Typed storage for derived data.

use std::{collections::HashMap, str::FromStr, sync::Arc};

use tokio::sync::RwLock;
use tycho_simulation::tycho_common::models::Address;

use super::{
    computation::{FailedItem, FailedItemError},
    types::{
        PoolDepthKey, PoolDepths, SpotPriceKey, SpotPrices, TokenGasPriceKey, TokenGasPrices,
        TokenPricesWithDeps,
    },
};
use crate::derived::SharedDerivedDataRef;

/// A computed value paired with the block it was computed for.
#[derive(Debug)]
struct ComputedValue<T> {
    data: T,
    block: u64,
}

/// Typed storage for derived data computations.
///
/// Provides typed access to previously computed derived data.
/// Each field is `Option` to indicate whether the computation has run.
#[derive(Debug, Default)]
pub struct DerivedData {
    token_prices: Option<ComputedValue<TokenGasPrices>>,
    /// Persistent failure map: key → (block, error). Merged on incremental runs, replaced on full.
    token_prices_failed: HashMap<TokenGasPriceKey, (u64, FailedItemError)>,
    /// Token prices with path dependency tracking for incremental computation.
    token_prices_deps: Option<ComputedValue<TokenPricesWithDeps>>,
    pool_depths: Option<ComputedValue<PoolDepths>>,
    /// Persistent failure map: key → (block, error). Merged on incremental runs, replaced on full.
    pool_depths_failed: HashMap<PoolDepthKey, (u64, FailedItemError)>,
    spot_prices: Option<ComputedValue<SpotPrices>>,
    /// Persistent failure map: key → (block, error). Merged on incremental runs, replaced on full.
    spot_prices_failed: HashMap<SpotPriceKey, (u64, FailedItemError)>,
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

    /// Returns `true` if all derived data types has been computed at least once.
    pub fn derived_data_ready(&self) -> bool {
        self.token_prices_block().is_some() &&
            self.token_prices_deps_block().is_some() &&
            self.pool_depths_block().is_some() &&
            self.spot_prices_block().is_some()
    }

    // -------------------------------------------------------------------------
    // Token Prices
    // -------------------------------------------------------------------------

    /// Returns token prices if computed.
    pub fn token_prices(&self) -> Option<&TokenGasPrices> {
        self.token_prices
            .as_ref()
            .map(|v| &v.data)
    }

    /// Returns the block at which token prices were last computed.
    pub fn token_prices_block(&self) -> Option<u64> {
        self.token_prices
            .as_ref()
            .map(|v| v.block)
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
        let new_failures: HashMap<TokenGasPriceKey, (u64, FailedItemError)> = failed_items
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

        self.token_prices = Some(ComputedValue { data: prices, block });
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
        self.token_prices = None;
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
    // Pool Depths
    // -------------------------------------------------------------------------

    /// Returns pool depths if computed.
    pub fn pool_depths(&self) -> Option<&PoolDepths> {
        self.pool_depths
            .as_ref()
            .map(|v| &v.data)
    }

    /// Returns the block at which pool depths were last computed.
    pub fn pool_depths_block(&self) -> Option<u64> {
        self.pool_depths
            .as_ref()
            .map(|v| v.block)
    }

    /// Sets pool depths, merging failures for incremental runs.
    ///
    /// For full recomputes, the failure map is replaced entirely. For incremental runs,
    /// failures are merged: existing entries for keys that now succeed are removed, new
    /// failures are inserted, and entries for keys not attempted this run are preserved.
    pub fn set_pool_depths(
        &mut self,
        depths: PoolDepths,
        failed_items: Vec<FailedItem>,
        block: u64,
        is_full_recompute: bool,
    ) {
        let new_failures: HashMap<PoolDepthKey, (u64, FailedItemError)> = failed_items
            .into_iter()
            .filter_map(|f| parse_pair_key(&f.key).map(|k| (k, (block, f.error))))
            .collect();

        if is_full_recompute {
            self.pool_depths_failed = new_failures;
        } else {
            self.pool_depths_failed
                .retain(|k, _| !depths.contains_key(k));
            self.pool_depths_failed
                .extend(new_failures);
        }

        self.pool_depths = Some(ComputedValue { data: depths, block });
    }

    /// Returns `(block, error)` for this key if it failed in a past pool depth
    /// computation, or `None` if it succeeded or was not attempted.
    ///
    /// Key format: `(component_id, token_in, token_out)`
    pub fn pool_depth_failure(&self, key: &PoolDepthKey) -> Option<(u64, &FailedItemError)> {
        self.pool_depths_failed
            .get(key)
            .map(|(block, error)| (*block, error))
    }

    /// Clears pool depths and their failure map.
    pub fn clear_pool_depths(&mut self) {
        self.pool_depths = None;
        self.pool_depths_failed.clear();
    }

    // -------------------------------------------------------------------------
    // Spot Prices
    // -------------------------------------------------------------------------

    /// Returns spot prices if computed.
    pub fn spot_prices(&self) -> Option<&SpotPrices> {
        self.spot_prices
            .as_ref()
            .map(|v| &v.data)
    }

    /// Returns the block at which spot prices were last computed.
    pub fn spot_prices_block(&self) -> Option<u64> {
        self.spot_prices
            .as_ref()
            .map(|v| v.block)
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
        let new_failures: HashMap<SpotPriceKey, (u64, FailedItemError)> = failed_items
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

        self.spot_prices = Some(ComputedValue { data: prices, block });
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
        self.spot_prices = None;
        self.spot_prices_failed.clear();
    }

    // -------------------------------------------------------------------------
    // Bulk Operations
    // -------------------------------------------------------------------------

    /// Clears all stored data, including all failure maps.
    pub fn clear_all(&mut self) {
        self.token_prices = None;
        self.token_prices_failed.clear();
        self.token_prices_deps = None;
        self.pool_depths = None;
        self.pool_depths_failed.clear();
        self.spot_prices = None;
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
        assert_eq!(store.pool_depths_block(), None);
    }

    #[test]
    fn test_spot_prices_block_tracks_independently() {
        let mut store = DerivedData::new();
        store.set_spot_prices(Default::default(), vec![], 10, true);
        assert_eq!(store.spot_prices_block(), Some(10));
        assert_eq!(store.token_prices_block(), None);
    }

    #[test]
    fn test_pool_depths_block_tracks_independently() {
        let mut store = DerivedData::new();
        store.set_pool_depths(Default::default(), vec![], 7, true);
        assert_eq!(store.pool_depths_block(), Some(7));
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

        store.set_pool_depths(Default::default(), vec![], 9, true);
        assert!(store.derived_data_ready());
    }

    #[test]
    fn test_clear_all_resets_all_fields() {
        let mut store = DerivedData::new();
        store.set_token_prices(Default::default(), vec![], 1, true);
        store.set_spot_prices(Default::default(), vec![], 1, true);
        store.set_pool_depths(Default::default(), vec![], 1, true);

        store.clear_all();

        assert!(store.token_prices().is_none());
        assert!(store.spot_prices().is_none());
        assert!(store.pool_depths().is_none());
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
        let key = pair_key("pool1", 0x01, 0x02);
        let key_str = format!("pool1/{}/{}", addr(0x01), addr(0x02));
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
        assert_eq!(store.spot_price_failure(&pair_key("pool1", 0x01, 0x03)), None);
    }

    #[test]
    fn test_pool_depth_failure_stored_with_block() {
        let key: PoolDepthKey = pair_key("pool1", 0x01, 0x02);
        let key_str = format!("pool1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_pool_depths(
            Default::default(),
            vec![failed(
                &key_str,
                FailedItemError::SimulationFailed("depth error".into()),
            )],
            7,
            true,
        );
        assert_eq!(
            store.pool_depth_failure(&key),
            Some((7, &FailedItemError::SimulationFailed("depth error".into())))
        );
        assert_eq!(store.pool_depth_failure(&pair_key("pool2", 0x01, 0x02)), None);
    }

    #[test]
    fn test_rerunning_with_empty_failures_clears_old_reasons() {
        let key = pair_key("pool1", 0x01, 0x02);
        let key_str = format!("pool1/{}/{}", addr(0x01), addr(0x02));
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
        let key = pair_key("pool1", 0x01, 0x02);
        let key_str = format!("pool1/{}/{}", addr(0x01), addr(0x02));
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
    fn test_clear_pool_depths_clears_failure_map() {
        let key: PoolDepthKey = pair_key("pool1", 0x01, 0x02);
        let key_str = format!("pool1/{}/{}", addr(0x01), addr(0x02));
        let mut store = DerivedData::new();
        store.set_pool_depths(
            Default::default(),
            vec![failed(&key_str, FailedItemError::MissingSpotPrice)],
            1,
            true,
        );
        store.clear_pool_depths();
        assert_eq!(store.pool_depth_failure(&key), None);
    }

    #[test]
    fn test_incremental_run_preserves_failures_for_unattempted_items() {
        let key_a = pair_key("pool_a", 0x01, 0x02);
        let key_a_str = format!("pool_a/{}/{}", addr(0x01), addr(0x02));
        let key_b = pair_key("pool_b", 0x03, 0x04);
        let key_b_str = format!("pool_b/{}/{}", addr(0x03), addr(0x04));

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

        // Incremental run at block 11: only pool_b is attempted and succeeds
        let mut prices = SpotPrices::default();
        prices.insert(key_b.clone(), 1.0);
        store.set_spot_prices(prices, vec![], 11, false);

        // pool_a was not attempted — failure is preserved from block 10
        assert_eq!(
            store.spot_price_failure(&key_a),
            Some((10, &FailedItemError::MissingSimulationState))
        );
        // pool_b succeeded — failure is cleared
        assert_eq!(store.spot_price_failure(&key_b), None);
    }

    #[test]
    fn test_incremental_run_updates_block_on_repeated_failure() {
        let key = pair_key("pool_a", 0x01, 0x02);
        let key_str = format!("pool_a/{}/{}", addr(0x01), addr(0x02));

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

        // Incremental run at block 11: pool_a fails again with a new error
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
        let pair = pair_key("pool1", 0x01, 0x02);
        let pair_str = format!("pool1/{}/{}", addr(0x01), addr(0x02));

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
        store.set_pool_depths(
            Default::default(),
            vec![failed(&pair_str, FailedItemError::MissingSpotPrice)],
            1,
            true,
        );

        store.clear_all();

        assert_eq!(store.token_price_failure(&token_addr), None);
        assert_eq!(store.spot_price_failure(&pair), None);
        assert_eq!(store.pool_depth_failure(&pair), None);
    }
}
