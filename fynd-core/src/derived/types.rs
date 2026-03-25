//! Data types for derived computations.

use std::collections::{HashMap, HashSet};

use num_bigint::BigUint;
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::types::ComponentId;

// =============================================================================
// Spot Price Types
// =============================================================================

/// Key for spot price lookups: (component_id, token_in, token_out).
///
/// Uniquely identifies a directional price within a specific pool.
pub type SpotPriceKey = (ComponentId, Address, Address);

/// Spot prices map: key -> spot price as f64.
///
/// Represents: 1 token_in = spot_price token_out.
pub type SpotPrices = HashMap<SpotPriceKey, f64>;

// =============================================================================
// Pool Depth Types
// =============================================================================

/// Key for pool depth lookups: (component_id, token_in, token_out).
///
/// Uniquely identifies a directional liquidity depth within a specific pool.
pub type PoolDepthKey = (ComponentId, Address, Address);

/// Pool depths map: key -> maximum input amount at the configured slippage threshold.
///
/// Represents how much can be traded before the specified price impact.
pub type PoolDepths = HashMap<PoolDepthKey, BigUint>;

// =============================================================================
// Token Gas Price Types
// =============================================================================

/// Key for token price lookups: token address.
pub type TokenGasPriceKey = Address;

/// Token prices map: token address -> it's mid-price relative to gas token.
pub type TokenGasPrices = HashMap<TokenGasPriceKey, Price>;

/// Token price with path dependency tracking for incremental computation.
///
/// Tracks which components (pools) were used in the selected path,
/// enabling selective recomputation when only specific pools change.
#[derive(Debug, Clone)]
pub struct TokenPriceEntry {
    /// The computed mid-price relative to gas token.
    pub price: Price,
    /// Components (pool IDs) from all candidate paths considered for this token.
    ///
    /// Used for invalidation: if any of these components change,
    /// this token's price needs recomputation. Includes pools from all discovered
    /// paths, not just the selected best path, so a change in any competing pool
    /// triggers recomputation.
    pub path_components: HashSet<ComponentId>,
}

/// Token prices with path dependency tracking.
///
/// Used internally by `TokenGasPriceComputation` to enable incremental updates.
pub type TokenPricesWithDeps = HashMap<Address, TokenPriceEntry>;
