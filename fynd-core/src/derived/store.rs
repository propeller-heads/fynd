//! Typed storage for derived data.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::types::{PoolDepths, SpotPrices, TokenGasPrices, TokenPricesWithDeps};
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
    /// Token prices with path dependency tracking for incremental computation.
    token_prices_deps: Option<ComputedValue<TokenPricesWithDeps>>,
    pool_depths: Option<ComputedValue<PoolDepths>>,
    spot_prices: Option<ComputedValue<SpotPrices>>,
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

    /// Returns the block number at which any data was last computed.
    ///
    /// Returns the maximum block across all computed values, so it returns `Some` as long as
    /// any computation has run. Used by health checks.
    pub fn last_block(&self) -> Option<u64> {
        [
            self.token_prices_block(),
            self.token_prices_deps_block(),
            self.pool_depths_block(),
            self.spot_prices_block(),
        ]
        .into_iter()
        .flatten()
        .max()
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

    /// Sets token prices.
    pub fn set_token_prices(&mut self, prices: TokenGasPrices, block: u64) {
        self.token_prices = Some(ComputedValue { data: prices, block });
    }

    /// Clears token prices.
    pub fn clear_token_prices(&mut self) {
        self.token_prices = None;
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

    /// Sets pool depths.
    pub fn set_pool_depths(&mut self, depths: PoolDepths, block: u64) {
        self.pool_depths = Some(ComputedValue { data: depths, block });
    }

    /// Clears pool depths.
    pub fn clear_pool_depths(&mut self) {
        self.pool_depths = None;
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

    /// Sets spot prices.
    pub fn set_spot_prices(&mut self, prices: SpotPrices, block: u64) {
        self.spot_prices = Some(ComputedValue { data: prices, block });
    }

    /// Clears spot prices.
    pub fn clear_spot_prices(&mut self) {
        self.spot_prices = None;
    }

    // -------------------------------------------------------------------------
    // Bulk Operations
    // -------------------------------------------------------------------------

    /// Clears all stored data.
    pub fn clear_all(&mut self) {
        self.token_prices = None;
        self.token_prices_deps = None;
        self.pool_depths = None;
        self.spot_prices = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_prices_block_tracks_independently() {
        let mut store = DerivedData::new();
        assert_eq!(store.token_prices_block(), None);

        store.set_token_prices(Default::default(), 42);
        assert_eq!(store.token_prices_block(), Some(42));

        // Other computations not set yet
        assert_eq!(store.spot_prices_block(), None);
        assert_eq!(store.pool_depths_block(), None);
    }

    #[test]
    fn spot_prices_block_tracks_independently() {
        let mut store = DerivedData::new();
        store.set_spot_prices(Default::default(), 10);
        assert_eq!(store.spot_prices_block(), Some(10));
        assert_eq!(store.token_prices_block(), None);
    }

    #[test]
    fn pool_depths_block_tracks_independently() {
        let mut store = DerivedData::new();
        store.set_pool_depths(Default::default(), 7);
        assert_eq!(store.pool_depths_block(), Some(7));
        assert_eq!(store.token_prices_block(), None);
    }

    #[test]
    fn last_block_returns_max_across_computations() {
        let mut store = DerivedData::new();
        assert_eq!(store.last_block(), None);

        store.set_spot_prices(Default::default(), 5);
        assert_eq!(store.last_block(), Some(5));

        store.set_token_prices(Default::default(), 10);
        assert_eq!(store.last_block(), Some(10));

        store.set_pool_depths(Default::default(), 9);
        assert_eq!(store.last_block(), Some(10)); // max is still 10
    }

    #[test]
    fn clear_all_resets_all_fields() {
        let mut store = DerivedData::new();
        store.set_token_prices(Default::default(), 1);
        store.set_spot_prices(Default::default(), 1);
        store.set_pool_depths(Default::default(), 1);

        store.clear_all();

        assert!(store.token_prices().is_none());
        assert!(store.spot_prices().is_none());
        assert!(store.pool_depths().is_none());
        assert_eq!(store.last_block(), None);
    }
}
