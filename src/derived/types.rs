//! Data types for derived computations.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Address;

use crate::types::ComponentId;

/// Price of a token relative to the gas token (e.g., ETH).
///
/// Used for gas cost estimation in output token terms.
#[derive(Debug, Clone)]
pub struct TokenPrice {
    /// Token address.
    pub token: Address,
    /// Price in gas token units (e.g., 1 USDC = 0.0005 ETH).
    /// Represented as a ratio: price = numerator / denominator.
    pub numerator: BigUint,
    pub denominator: BigUint,
}

impl TokenPrice {
    pub fn new(token: Address, numerator: BigUint, denominator: BigUint) -> Self {
        Self { token, numerator, denominator }
    }
}

/// Liquidity depth for a pool at a specific price level.
///
/// Represents how much can be traded before significant price impact.
#[derive(Debug, Clone)]
pub struct PoolDepth {
    /// Component (pool) identifier.
    pub component_id: ComponentId,
    /// Token being sold.
    pub token_in: Address,
    /// Token being bought.
    pub token_out: Address,
    /// Amount available at current price level.
    pub available_amount: BigUint,
}

impl PoolDepth {
    pub fn new(
        component_id: ComponentId,
        token_in: Address,
        token_out: Address,
        available_amount: BigUint,
    ) -> Self {
        Self { component_id, token_in, token_out, available_amount }
    }
}

/// Spot price for a specific pool and token pair.
///
/// The instantaneous exchange rate without price impact.
#[derive(Debug, Clone)]
pub struct SpotPrice {
    /// Component (pool) identifier.
    pub component_id: ComponentId,
    /// Token being sold.
    pub token_in: Address,
    /// Token being bought.
    pub token_out: Address,
    /// Spot price as ratio: price = numerator / denominator.
    /// "1 token_in = (numerator/denominator) token_out"
    pub numerator: BigUint,
    pub denominator: BigUint,
}

impl SpotPrice {
    pub fn new(
        component_id: ComponentId,
        token_in: Address,
        token_out: Address,
        numerator: BigUint,
        denominator: BigUint,
    ) -> Self {
        Self { component_id, token_in, token_out, numerator, denominator }
    }
}
