//! Typed storage for derived data.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::types::{PoolDepths, SpotPrices, TokenGasPrices, TokenPricesWithDeps};
use crate::derived::SharedDerivedDataRef;

/// Typed storage for derived data computations.
///
/// Provides typed access to previously computed derived data.
/// Each field is `Option` to indicate whether the computation has run.
#[derive(Debug, Default)]
pub struct DerivedData {
    token_prices: Option<TokenGasPrices>,
    /// Token prices with path dependency tracking for incremental computation.
    token_prices_deps: Option<TokenPricesWithDeps>,
    pool_depths: Option<PoolDepths>,
    spot_prices: Option<SpotPrices>,
    /// Block number at which data was last computed.
    last_block: Option<u64>,
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

    /// Returns the block number at which data was last computed.
    pub fn last_block(&self) -> Option<u64> {
        self.last_block
    }

    // -------------------------------------------------------------------------
    // Token Prices
    // -------------------------------------------------------------------------

    /// Returns token prices if computed.
    pub fn token_prices(&self) -> Option<&TokenGasPrices> {
        self.token_prices.as_ref()
    }

    /// Sets token prices.
    pub fn set_token_prices(&mut self, prices: TokenGasPrices, block: u64) {
        self.token_prices = Some(prices);
        self.last_block = Some(block);
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
        self.token_prices_deps.as_ref()
    }

    /// Sets token prices with path dependencies.
    pub fn set_token_prices_deps(&mut self, prices: TokenPricesWithDeps, block: u64) {
        self.token_prices_deps = Some(prices);
        self.last_block = Some(block);
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
        self.pool_depths.as_ref()
    }

    /// Sets pool depths.
    pub fn set_pool_depths(&mut self, depths: PoolDepths, block: u64) {
        self.pool_depths = Some(depths);
        self.last_block = Some(block);
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
        self.spot_prices.as_ref()
    }

    /// Sets spot prices.
    pub fn set_spot_prices(&mut self, prices: SpotPrices, block: u64) {
        self.spot_prices = Some(prices);
        self.last_block = Some(block);
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
        self.last_block = None;
    }
}
