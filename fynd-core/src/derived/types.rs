//! Data types for derived computations.

use num_bigint::BigUint;
use rustc_hash::{FxHashMap, FxHashSet};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::types::ComponentId;

// =============================================================================
// Spot Price Types
// =============================================================================

/// Key for spot price lookups: (component_id, token_in, token_out).
///
/// Uniquely identifies a directional price within a specific component.
pub type SpotPriceKey = (ComponentId, Address, Address);

/// Spot prices map: key -> spot price as f64.
///
/// Represents: 1 token_in = spot_price token_out.
pub type SpotPrices = FxHashMap<SpotPriceKey, f64>;

// =============================================================================
// Component Depth Types
// =============================================================================

/// Key for component depth lookups: (component_id, token_in, token_out).
///
/// Uniquely identifies a directional liquidity depth within a specific component.
pub type ComponentDepthKey = (ComponentId, Address, Address);

/// Component depths map: key -> maximum input amount at the configured slippage threshold.
///
/// Represents how much can be traded before the specified price impact.
pub type ComponentDepths = FxHashMap<ComponentDepthKey, BigUint>;

// =============================================================================
// Token Gas Price Types
// =============================================================================

/// Key for token price lookups: token address.
pub type TokenGasPriceKey = Address;

/// Token prices map: token address -> it's mid-price relative to gas token.
pub type TokenGasPrices = FxHashMap<TokenGasPriceKey, Price>;

/// Token price with path dependency tracking for incremental computation.
///
/// Tracks which components were used in the selected path,
/// enabling selective recomputation when only specific components change.
#[derive(Debug, Clone)]
pub struct TokenPriceEntry {
    /// The computed mid-price relative to gas token.
    pub price: Price,
    /// Component IDs from all candidate paths considered for this token.
    ///
    /// Used for invalidation: if any of these components change,
    /// this token's price needs recomputation. Includes components from all discovered
    /// paths, not just the selected best path, so a change in any competing component
    /// triggers recomputation.
    pub path_components: FxHashSet<ComponentId>,
}

/// Token prices with path dependency tracking.
///
/// Used internally by `TokenGasPriceComputation` to enable incremental updates.
pub type TokenPricesWithDeps = FxHashMap<TokenGasPriceKey, TokenPriceEntry>;

/// A token's two directional rates, kept when `PricingMode::Mid` solves the sell side.
///
/// Both are token units per one gas-token unit. An entry always has both rates from one solve;
/// a token that could not be solved in both directions is absent here, though its buy-only
/// price may still be in the price map.
#[derive(Debug, Clone)]
pub struct TokenPriceDirections {
    /// What one gas-token unit buys via the best route.
    pub buy: Price,
    /// What selling that output back implies.
    pub sell: Price,
}

/// Direction rates per token address.
pub type TokenGasPriceDirections = FxHashMap<TokenGasPriceKey, TokenPriceDirections>;

/// Output of the token gas price computation: the price map plus, in `Mid` mode, the
/// direction rates it was derived from.
#[derive(Debug, Clone)]
pub struct TokenPricesOutput {
    /// The buy rate per token, in every mode.
    pub prices: TokenGasPrices,
    /// Both rates per token; `None` in `BuyOnly` mode.
    pub directions: Option<TokenGasPriceDirections>,
}
