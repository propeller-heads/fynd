//! Types and helpers for the GET /v1/prices endpoint.

use std::fmt;

use fynd_core::types::ComponentId;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;
use utoipa::{IntoParams, ToSchema};

/// Version of the unit contract used by [`TokenPriceEntry::price`].
pub const PRICE_UNIT_CONTRACT_V1: &str = "PRICE_UNIT_CONTRACT_V1";

/// Machine-readable unit name for [`TokenPriceEntry::price`].
pub const RAW_TOKEN_UNITS_PER_RAW_GAS_UNIT: &str = "raw_token_units_per_raw_gas_unit";

/// Query parameters for GET /v1/prices.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct PricesQuery {
    /// Comma-separated list of additional data to include.
    /// Valid values: `depths`, `spot_prices`.
    #[param(example = "depths,spot_prices")]
    pub include: Option<String>,
    /// Maximum number of spot_prices and component_depths entries (default: 1000).
    #[param(example = 1000)]
    pub limit: Option<usize>,
}

/// Parsed variant of the `include` query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeField {
    /// Include component depth data.
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

/// Block numbers at which each computation was last run.
#[derive(Debug, Serialize, ToSchema)]
pub struct ComputationBlocks {
    /// Block at which token gas prices were computed.
    pub token_prices: u64,
    /// Block at which spot prices were computed. `None` if not yet available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot_prices: Option<u64>,
    /// Block at which component depths were computed. `None` if not yet available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_depths: Option<u64>,
}

/// Top-level response for GET /v1/prices.
#[derive(Debug, Serialize, ToSchema)]
pub struct PricesResponse {
    /// Token gas prices relative to the native gas token.
    pub prices: Vec<TokenPriceEntry>,
    /// The gas token address (e.g. WETH).
    #[schema(value_type = String, example = "0x0000000000000000000000000000000000000000")]
    pub gas_token: Address,
    /// Block numbers at which each computation was last run.
    pub blocks: ComputationBlocks,
    /// Spot prices per component direction (only if requested via `include=spot_prices`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot_prices: Option<Vec<SpotPriceEntry>>,
    /// Component depths per component direction (only if requested via `include=depths`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_depths: Option<Vec<ComponentDepthEntry>>,
}

/// A single token's gas price.
#[derive(Debug, Serialize, ToSchema)]
pub struct TokenPriceEntry {
    /// Token address.
    #[schema(value_type = String, example = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    pub token: Address,
    /// `rawTokenUnitsPerRawGasUnit`: raw target-token units divided by raw gas-token units.
    ///
    /// This approximate `f64` follows `PRICE_UNIT_CONTRACT_V1` and is intended only for display
    /// and analytics. Consumers must normalize both tokens' decimals before using it.
    #[schema(example = 0.000000003)]
    pub price: f64,
}

/// A single directional spot price within a component (liquidity pool).
#[derive(Debug, Serialize, ToSchema)]
pub struct SpotPriceEntry {
    /// Component (liquidity pool) identifier.
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

/// A single directional component depth.
#[derive(Debug, Serialize, ToSchema)]
pub struct ComponentDepthEntry {
    /// Component (liquidity pool) identifier.
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
/// Returns `None` unless the resulting value is finite, strictly positive, and representable as
/// `f64` without underflowing to zero.
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
    let price = numerator.to_f64()? / denominator.to_f64()?;
    if price.is_finite() && price > 0.0 {
        Some(price)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use num_bigint::BigUint;
    use serde::Deserialize;

    use super::*;

    const UNIT_CONTRACT_FIXTURE: &str =
        include_str!("../../tests/fixtures/v1-prices-unit-contract-v1.json");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UnitContractFixture {
        contract_version: String,
        price_unit: String,
        gas_token: FixtureToken,
        anchor_usd_assumption: String,
        cases: Vec<FixtureCase>,
    }

    #[derive(Deserialize)]
    struct FixtureToken {
        address: String,
        decimals: Option<u16>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureCase {
        name: String,
        expectation: String,
        token: FixtureToken,
        price_fraction: Option<FixtureFraction>,
        raw_price: String,
        duplicate_of: Option<String>,
        response_gas_token: Option<String>,
        expected_error: Option<String>,
    }

    #[derive(Deserialize)]
    struct FixtureFraction {
        numerator: String,
        denominator: String,
    }

    fn unit_contract_fixture() -> UnitContractFixture {
        serde_json::from_str(UNIT_CONTRACT_FIXTURE)
            .expect("unit contract fixture must be valid JSON")
    }

    #[test]
    fn test_price_unit_contract_fixture_exercises_all_cases() {
        let fixture = unit_contract_fixture();

        assert_eq!(fixture.contract_version, PRICE_UNIT_CONTRACT_V1);
        assert_eq!(fixture.price_unit, RAW_TOKEN_UNITS_PER_RAW_GAS_UNIT);
        assert_eq!(fixture.gas_token.decimals, Some(18));
        assert_eq!(fixture.anchor_usd_assumption, "1");

        let names: HashSet<&str> = fixture
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect();
        for required in [
            "6_over_18_anchor",
            "8_over_18_token",
            "18_over_18_token",
            "missing_decimals",
            "zero_price",
            "negative_price",
            "nan_price",
            "positive_infinity_price",
            "negative_infinity_price",
            "overflow_scale",
            "duplicate_address",
            "gas_token_mismatch",
        ] {
            assert!(names.contains(required), "missing fixture case {required}");
        }

        let mut seen_addresses = HashSet::new();
        for case in &fixture.cases {
            let result = validate_fixture_case(case, &fixture, &mut seen_addresses);
            match case.expectation.as_str() {
                "accepted" => assert_eq!(result, Ok(()), "{}", case.name),
                "rejected" => assert_eq!(
                    result,
                    Err(case
                        .expected_error
                        .as_deref()
                        .expect("rejected case needs an error")),
                    "{}",
                    case.name
                ),
                other => panic!("unknown fixture expectation {other}"),
            }
        }
    }

    fn validate_fixture_case<'a>(
        case: &'a FixtureCase,
        fixture: &UnitContractFixture,
        seen_addresses: &mut HashSet<String>,
    ) -> Result<(), &'a str> {
        let Some(decimals) = case.token.decimals else { return Err("missing_decimals") };
        if decimals > u8::MAX.into() {
            return Err("decimal_scale_out_of_range")
        }

        let address = case.token.address.to_ascii_lowercase();
        if !seen_addresses.insert(address) {
            assert!(case.duplicate_of.is_some(), "duplicate case must name its original");
            return Err("duplicate_address")
        }

        if let Some(response_gas_token) = &case.response_gas_token {
            if !response_gas_token.eq_ignore_ascii_case(&fixture.gas_token.address) {
                return Err("gas_token_mismatch")
            }
        }

        if let Some(fraction) = &case.price_fraction {
            let numerator = BigUint::from_str(&fraction.numerator).unwrap();
            let denominator = BigUint::from_str(&fraction.denominator).unwrap();
            if price_to_f64(&numerator, &denominator).is_none() {
                return Err("non_positive_price")
            }
        } else {
            let price = case.raw_price.parse::<f64>().unwrap();
            if !price.is_finite() {
                return Err("non_finite_price")
            }
            if price <= 0.0 {
                return Err("non_positive_price")
            }
        }

        Ok(())
    }

    #[test]
    fn test_price_unit_contract_fixture_matches_real_serializer() {
        let fixture = unit_contract_fixture();

        for case in fixture
            .cases
            .iter()
            .filter(|case| case.expectation == "accepted")
        {
            let fraction = case
                .price_fraction
                .as_ref()
                .expect("accepted cases need an exact price fraction");
            let numerator = BigUint::from_str(&fraction.numerator).unwrap();
            let denominator = BigUint::from_str(&fraction.denominator).unwrap();
            let price = price_to_f64(&numerator, &denominator).unwrap();
            let entry =
                TokenPriceEntry { token: Address::from_str(&case.token.address).unwrap(), price };
            let serialized = serde_json::to_value(entry).unwrap();

            assert_eq!(serialized["price"].to_string(), case.raw_price, "{}", case.name);
        }
    }

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
    fn price_to_f64_zero_numerator() {
        assert!(price_to_f64(&BigUint::from(0u8), &BigUint::from(1u8)).is_none());
    }

    #[test]
    fn price_to_f64_overflow() {
        let numerator = BigUint::from(10u8).pow(400);
        assert!(price_to_f64(&numerator, &BigUint::from(1u8)).is_none());
    }

    #[test]
    fn price_to_f64_underflow() {
        let denominator = BigUint::from(10u8).pow(400);
        assert!(price_to_f64(&BigUint::from(1u8), &denominator).is_none());
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
