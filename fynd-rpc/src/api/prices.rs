//! Types and helpers for the GET /v1/prices endpoint.

use std::fmt;

use fynd_core::types::ComponentId;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;
use utoipa::{IntoParams, ToSchema};

/// Query parameters for GET /v1/prices.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct PricesQuery {
    /// Comma-separated list of additional data to include.
    /// Valid values: `depths`, `spot_prices`.
    #[param(example = "depths,spot_prices")]
    pub include: Option<String>,
    /// Maximum number of spot_prices and pool_depths entries (default: 1000).
    #[param(example = 1000)]
    pub limit: Option<usize>,
}

/// Parsed variant of the `include` query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeField {
    /// Include pool depth data.
    Depths,
    /// Include spot price data.
    SpotPrices,
}

impl IncludeField {
    /// Parses a comma-separated include string into validated fields.
    ///
    /// Returns an error with the first unrecognised value.
    pub fn parse_include(raw: &str) -> Result<Vec<Self>, String> {
        let mut fields = Vec::new();
        for part in raw.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            match trimmed {
                "depths" => fields.push(Self::Depths),
                "spot_prices" => fields.push(Self::SpotPrices),
                other => {
                    return Err(format!(
                        "unknown include field '{}'. Valid values: depths, spot_prices",
                        other,
                    ));
                }
            }
        }
        Ok(fields)
    }
}

impl fmt::Display for IncludeField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Depths => write!(f, "depths"),
            Self::SpotPrices => write!(f, "spot_prices"),
        }
    }
}

/// Top-level response for GET /v1/prices.
#[derive(Debug, Serialize, ToSchema)]
pub struct PricesResponse {
    /// Token gas prices relative to the native gas token.
    pub prices: Vec<TokenPriceEntry>,
    /// The gas token address (e.g. WETH).
    #[schema(value_type = String, example = "0x0000000000000000000000000000000000000000")]
    pub gas_token: Address,
    /// Block number at which prices were last computed.
    pub last_block: u64,
    /// Spot prices per pool direction (only if requested via `include=spot_prices`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot_prices: Option<Vec<SpotPriceEntry>>,
    /// Pool depths per pool direction (only if requested via `include=depths`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_depths: Option<Vec<PoolDepthEntry>>,
}

/// A single token's gas price.
#[derive(Debug, Serialize, ToSchema)]
pub struct TokenPriceEntry {
    /// Token address.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token: Address,
    /// Price relative to gas token as a float.
    pub price: f64,
}

/// A single directional spot price within a pool.
#[derive(Debug, Serialize, ToSchema)]
pub struct SpotPriceEntry {
    /// Pool / component identifier.
    pub component_id: ComponentId,
    /// Input token address.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub token_in: Address,
    /// Output token address.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token_out: Address,
    /// Spot price (1 token_in = price token_out).
    pub price: f64,
}

/// A single directional pool depth.
#[derive(Debug, Serialize, ToSchema)]
pub struct PoolDepthEntry {
    /// Pool / component identifier.
    pub component_id: ComponentId,
    /// Input token address.
    #[schema(value_type = String, example = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    pub token_in: Address,
    /// Output token address.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token_out: Address,
    /// Maximum input amount before hitting the slippage threshold (decimal string).
    pub depth: String,
}

/// Convert a `tycho_core::Price { numerator, denominator }` to f64.
///
/// Returns `None` if the denominator is zero or if either value overflows f64.
/// Note: f64 prices are approximate and suitable for TVL calculations,
/// not for execution-critical amounts.
pub fn price_to_f64(
    numerator: &num_bigint::BigUint,
    denominator: &num_bigint::BigUint,
) -> Option<f64> {
    use num_traits::{ToPrimitive, Zero};

    if denominator.is_zero() {
        return None;
    }
    let n = numerator.to_f64()?;
    let d = denominator.to_f64()?;
    Some(n / d)
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::*;

    // ---- IncludeField parsing ----

    #[test]
    fn parse_include_empty() {
        assert_eq!(IncludeField::parse_include("").unwrap(), vec![]);
    }

    #[test]
    fn parse_include_depths() {
        let fields = IncludeField::parse_include("depths").unwrap();
        assert_eq!(fields, vec![IncludeField::Depths]);
    }

    #[test]
    fn parse_include_spot_prices() {
        let fields = IncludeField::parse_include("spot_prices").unwrap();
        assert_eq!(fields, vec![IncludeField::SpotPrices]);
    }

    #[test]
    fn parse_include_both() {
        let fields = IncludeField::parse_include("depths,spot_prices").unwrap();
        assert_eq!(fields, vec![IncludeField::Depths, IncludeField::SpotPrices]);
    }

    #[test]
    fn parse_include_with_whitespace() {
        let fields = IncludeField::parse_include(" depths , spot_prices ").unwrap();
        assert_eq!(fields, vec![IncludeField::Depths, IncludeField::SpotPrices]);
    }

    #[test]
    fn parse_include_unknown_rejects() {
        let err = IncludeField::parse_include("depths,foobar").unwrap_err();
        assert!(err.contains("foobar"));
    }

    // ---- Price to f64 conversion ----

    #[test]
    fn price_to_f64_normal() {
        let n = BigUint::from(3u64);
        let d = BigUint::from(10u64);
        let result = price_to_f64(&n, &d).unwrap();
        assert!((result - 0.3).abs() < 1e-10);
    }

    #[test]
    fn price_to_f64_zero_denominator() {
        let n = BigUint::from(1u64);
        let d = BigUint::from(0u64);
        assert!(price_to_f64(&n, &d).is_none());
    }

    #[test]
    fn price_to_f64_large_values() {
        let n = BigUint::from(10u64).pow(18);
        let d = BigUint::from(10u64).pow(18);
        let result = price_to_f64(&n, &d).unwrap();
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    fn price_to_f64_small_fraction() {
        let n = BigUint::from(1u64);
        let d = BigUint::from(10u64).pow(6);
        let result = price_to_f64(&n, &d).unwrap();
        assert!((result - 1e-6).abs() < 1e-15);
    }
}
