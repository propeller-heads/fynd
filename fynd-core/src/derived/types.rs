//! Data types for derived computations.

use num_bigint::BigUint;
use num_traits::Zero;
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

/// Token prices map: token address → its mid-price, the mean of its buy and sell rates — a
/// token's exit cost is already reflected in its price.
///
/// A price is in token raw units per *native* gas raw unit, the unit `gas * gas_price` counts,
/// not per raw unit of the routable gas token it was measured against. The two differ on a
/// shared-balance chain such as Arc (18 native decimals against 6); see
/// [`GasTokenConfig`](crate::types::constants::GasTokenConfig).
pub type TokenGasPrices = FxHashMap<TokenGasPriceKey, Price>;

/// Converts `gas` units at `gas_price_wei` into raw units of a token using its derived price.
///
/// Division rounds up so a fractional raw unit never understates the gas cost. Returns `None`
/// for a zero price denominator, which every caller treats as an unpriced token.
pub(crate) fn gas_cost_in_token(
    gas: &BigUint,
    gas_price_wei: &BigUint,
    price: &Price,
) -> Option<BigUint> {
    mul_div_ceil(&(gas * gas_price_wei), &price.numerator, &price.denominator)
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
    if denominator.is_zero() {
        return None;
    }
    let scaled = value * multiplier;
    Some((scaled + denominator - BigUint::from(1u8)) / denominator)
}

/// Token price with the components that must re-price it when they change.
#[derive(Debug, Clone)]
pub struct TokenPriceEntry {
    /// The computed mid-price, in token raw units per native gas raw unit (see
    /// [`TokenGasPrices`]).
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

    fn tiny_price() -> Price {
        Price { numerator: BigUint::from(1u8), denominator: BigUint::from(10u64).pow(12) }
    }

    #[test]
    fn test_mul_div_ceil() {
        let ceil = |value: u8, multiplier: u8, denominator: u8| {
            mul_div_ceil(&value.into(), &multiplier.into(), &denominator.into())
        };

        assert_eq!(ceil(1, 11, 10), Some(BigUint::from(2u8)));
        assert_eq!(ceil(2, 10, 5), Some(BigUint::from(4u8)));
        assert_eq!(ceil(0, 10, 3), Some(BigUint::ZERO));
        assert_eq!(ceil(1, 1, 0), None);
    }

    #[test]
    fn test_gas_cost_in_token_fractional_raw_unit() {
        let price = tiny_price();
        let one = BigUint::from(1u8);

        assert_eq!(gas_cost_in_token(&one, &one, &price), Some(BigUint::from(1u8)));
        assert_eq!(
            gas_cost_in_token(&BigUint::from(10u64).pow(6), &BigUint::from(10u64).pow(6), &price),
            Some(BigUint::from(1u8))
        );
        assert_eq!(gas_cost_in_token(&BigUint::ZERO, &one, &price), Some(BigUint::ZERO));
    }

    #[test]
    fn test_gas_cost_in_token_zero_denominator() {
        let price = Price { numerator: BigUint::from(1u8), denominator: BigUint::ZERO };

        assert_eq!(gas_cost_in_token(&BigUint::from(1u8), &BigUint::from(1u8), &price), None);
    }
}
