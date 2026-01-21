//! Typed storage for derived data.

use std::collections::HashMap;

use tycho_simulation::tycho_common::models::Address;

use super::types::{PoolDepth, SpotPrice, TokenGasPrice};
use crate::types::ComponentId;

/// Type alias for token prices map.
pub type TokenPrices = HashMap<Address, TokenGasPrice>;

/// Type alias for pool depths map.
/// Key: (component_id, token_in, token_out)
pub type PoolDepths = HashMap<(ComponentId, Address, Address), PoolDepth>;

/// Type alias for spot prices map.
/// Key: (component_id, token_in, token_out)
pub type SpotPrices = HashMap<(ComponentId, Address, Address), SpotPrice>;

/// Typed storage for derived data computations.
///
/// Provides typed access to previously computed derived data.
/// Each field is `Option` to indicate whether the computation has run.
#[derive(Debug, Default)]
pub struct DerivedDataStore {
    token_prices: Option<TokenPrices>,
    pool_depths: Option<PoolDepths>,
    spot_prices: Option<SpotPrices>,
    /// Block number at which data was last computed.
    last_block: u64,
}

impl DerivedDataStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the block number at which data was last computed.
    pub fn last_block(&self) -> u64 {
        self.last_block
    }

    // -------------------------------------------------------------------------
    // Token Prices
    // -------------------------------------------------------------------------

    /// Returns token prices if computed.
    pub fn token_prices(&self) -> Option<&TokenPrices> {
        self.token_prices.as_ref()
    }

    /// Sets token prices.
    pub fn set_token_prices(&mut self, prices: TokenPrices, block: u64) {
        self.token_prices = Some(prices);
        self.last_block = block;
    }

    /// Clears token prices.
    pub fn clear_token_prices(&mut self) {
        self.token_prices = None;
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
        self.last_block = block;
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
        self.last_block = block;
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
        self.pool_depths = None;
        self.spot_prices = None;
        self.last_block = 0;
    }
}
