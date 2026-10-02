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

/// Component depths map: key -> the input after which the pool's net marginal price has fallen by
/// the configured marginal price drop.
pub type ComponentDepths = FxHashMap<ComponentDepthKey, BigUint>;

// =============================================================================
// Token Gas Price Types
// =============================================================================

/// Key for token price lookups: token address.
pub type TokenGasPriceKey = Address;

/// Token prices map: token address → its mid-price relative to the gas token, the mean of
/// its buy and sell rates — a token's exit cost is already reflected in its price.
pub type TokenGasPrices = FxHashMap<TokenGasPriceKey, Price>;

/// Converts a native gas cost into raw units of a token using a derived token price.
///
/// Prices are exact fractions in token raw units per native gas raw unit. Division rounds up so a
/// fractional output-token atom never understates gas cost.
pub(crate) fn gas_cost_in_token(gas_cost: &BigUint, price: &Price) -> Option<BigUint> {
    mul_div_ceil(gas_cost, &price.numerator, &price.denominator)
}

/// Multiplies two unsigned integers, divides by `denominator`, and rounds any remainder up.
///
/// Returns `None` for a zero denominator. This keeps every gas-cost conversion and refinement on
/// the same conservative integer rounding rule.
pub(crate) fn mul_div_ceil(
    value: &BigUint,
    multiplier: &BigUint,
    denominator: &BigUint,
) -> Option<BigUint> {
    if denominator == &BigUint::from(0u8) {
        return None;
    }
    let scaled = value * multiplier;
    if scaled == BigUint::from(0u8) {
        return Some(scaled);
    }
    Some((scaled + denominator - BigUint::from(1u8)) / denominator)
}

/// Token price with the components that must re-price it when they change.
#[derive(Debug, Clone)]
pub struct TokenPriceEntry {
    /// The computed mid-price relative to gas token.
    pub price: Price,
    /// The components of the routes that priced the token.
    ///
    /// A state change on any of them makes the token eligible for a new price.
    pub path_components: FxHashSet<ComponentId>,
}

/// Token prices with path dependency tracking.
///
/// Used internally by `TokenGasPriceComputation` to enable incremental updates.
pub type TokenPricesWithDeps = FxHashMap<TokenGasPriceKey, TokenPriceEntry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_ceil_preserves_fractional_refined_gas_cost() {
        assert_eq!(
            mul_div_ceil(&BigUint::from(1u8), &BigUint::from(11u8), &BigUint::from(10u8),),
            Some(BigUint::from(2u8))
        );
    }

    #[test]
    fn gas_cost_conversion_rounds_up_fractional_output_atoms() {
        let price =
            Price { numerator: BigUint::from(1u8), denominator: BigUint::from(10u64).pow(12) };

        assert_eq!(gas_cost_in_token(&BigUint::from(1u8), &price), Some(BigUint::from(1u8)));
        assert_eq!(
            gas_cost_in_token(&BigUint::from(10u64).pow(12), &price),
            Some(BigUint::from(1u8))
        );
        assert_eq!(gas_cost_in_token(&BigUint::from(0u8), &price), Some(BigUint::from(0u8)));
    }

    #[test]
    fn gas_cost_conversion_rejects_zero_denominator() {
        let price = Price { numerator: BigUint::from(1u8), denominator: BigUint::from(0u8) };

        assert_eq!(gas_cost_in_token(&BigUint::from(1u8), &price), None);
    }
}
