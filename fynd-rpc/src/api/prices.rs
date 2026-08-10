//! Types and helpers for the GET /v1/prices endpoint.

use std::fmt;

use fynd_core::types::ComponentId;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;
use utoipa::{IntoParams, ToSchema};

/// Version of the unit contract used by [`TokenPriceEntry::price`].
///
/// Serialized in every [`PricesResponse`] so consumers can verify the contract at runtime.
pub const PRICE_UNIT_CONTRACT_V1: &str = "PRICE_UNIT_CONTRACT_V1";

/// Machine-readable unit name for [`TokenPriceEntry::price`].
///
/// Serialized in every [`PricesResponse`] so consumers can verify the unit at runtime.
pub const RAW_TOKEN_UNITS_PER_RAW_GAS_UNIT: &str = "raw_token_units_per_raw_gas_unit";

/// Maximum number of significant digits in the decimal-string representation of
/// [`TokenPriceEntry::price`].
///
/// 17 digits matches the precision of an IEEE-754 `f64` significand without inheriting its
/// encoding or rendering quirks. Values that need more precision should consume the raw
/// `numerator`/`denominator` from the server's derived-data layer directly.
const PRICE_DECIMAL_PRECISION: usize = 17;

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
    /// Unit contract version for `prices[].price`. Consumers should assert this matches their
    /// expected contract before interpreting prices.
    #[schema(example = "PRICE_UNIT_CONTRACT_V1")]
    pub contract_version: &'static str,
    /// Machine-readable unit name for `prices[].price`:
    /// `raw_token_units_per_raw_gas_unit` (raw target-token units divided by raw gas-token units).
    #[schema(example = "raw_token_units_per_raw_gas_unit")]
    pub price_unit: &'static str,
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
    /// `rawTokenUnitsPerRawGasUnit`: raw target-token units divided by raw gas-token units,
    /// serialized as a decimal string with up to 17 significant digits.
    ///
    /// Follows `PRICE_UNIT_CONTRACT_V1` and is intended only for display and analytics.
    /// Consumers must normalize both tokens' decimals before using it. The string format
    /// avoids `f64` rendering inconsistencies across languages (e.g. Rust's `1500.0` vs
    /// JavaScript's `"1500"`); parse it with a decimal-aware parser (BigDecimal, BigNumber, etc.).
    #[schema(value_type = String, example = "0.000000003")]
    pub price: String,
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

/// Maximum number of decimal digits in numerator or denominator before the value is
/// considered too large to process safely. Prevents unbounded long-division loops on
/// pathological inputs without relying on f64 representability.
const MAX_BIGUINT_DIGITS: usize = 400;

/// Convert a `tycho_core::Price { numerator, denominator }` to a decimal string.
///
/// Performs exact integer long-division and emits up to
/// [`PRICE_DECIMAL_PRECISION`] significant digits in plain decimal notation
/// (no scientific notation, no trailing `.0`).
///
/// Returns `None` when the numerator is zero, the denominator is zero, or either
/// operand exceeds [`MAX_BIGUINT_DIGITS`] decimal digits (guards against unbounded
/// computation). Validation is based on the exact `BigUint` inputs — no `f64`
/// conversion is involved.
///
/// # Precision policy
///
/// The exact rational `numerator / denominator` may have infinitely many decimal
/// digits. This function truncates (not rounds) after 17 significant digits, which
/// matches the precision of an IEEE-754 `f64` significand. Consumers needing exact
/// precision should access the server's raw `BigUint` numerator/denominator directly.
pub fn price_to_decimal_string(
    numerator: &num_bigint::BigUint,
    denominator: &num_bigint::BigUint,
) -> Option<String> {
    use num_traits::Zero;

    if denominator.is_zero() || numerator.is_zero() {
        return None;
    }

    // Guard against pathological inputs that would cause unbounded long-division
    // or produce strings far exceeding any practical need.
    if numerator.to_string().len() > MAX_BIGUINT_DIGITS ||
        denominator.to_string().len() > MAX_BIGUINT_DIGITS
    {
        return None;
    }

    Some(biguint_division_to_decimal_string(numerator, denominator, PRICE_DECIMAL_PRECISION))
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

/// Perform exact integer long-division and format the result as a plain decimal string
/// with at most `max_sig_digits` significant digits (trailing zeros removed from the
/// fractional part, no scientific notation).
///
/// Leading fractional zeros do not consume significant-digit budget: for a value like
/// 1/700 (0.00142...), the two leading zeros after the decimal point are skipped, and
/// the full 17 significant digits are produced.
///
/// Example: `(3, 1_000_000_000, 17)` -> `"0.000000003"`, `(1500, 1, 17)` -> `"1500"`.
fn biguint_division_to_decimal_string(
    numerator: &num_bigint::BigUint,
    denominator: &num_bigint::BigUint,
    max_sig_digits: usize,
) -> String {
    use num_bigint::BigUint;
    use num_traits::{ToPrimitive, Zero};

    let int_part = numerator / denominator;
    let remainder = numerator % denominator;
    let int_str = int_part.to_string();

    if remainder.is_zero() {
        // Exact integer result. Truncate to max significant digits if needed.
        return truncate_sig_digits(&int_str, max_sig_digits);
    }

    // Count significant digits from the integer part (after stripping leading zeros).
    let int_sig = int_str.trim_start_matches('0').len();
    let remaining_sig = max_sig_digits.saturating_sub(int_sig);

    if remaining_sig == 0 {
        // Integer part already consumes the full significant-digit budget.
        return truncate_sig_digits(&int_str, max_sig_digits);
    }

    // Long-division for the fractional part. We track significant digits explicitly:
    // leading zeros (before the first non-zero fractional digit) do not consume
    // budget. This ensures values like 1/700 produce the full 17 significant digits
    // and non-zero values with many leading fractional zeros never serialize as "0".
    let mut frac_digits = String::new();
    let mut rem = remainder;
    let ten = BigUint::from(10u8);
    let mut sig_count = 0usize;
    let mut hit_nonzero = false;

    while sig_count < remaining_sig {
        if rem.is_zero() {
            break;
        }
        rem *= &ten;
        let digit = &rem / denominator;
        rem = &rem % denominator;
        let d = char::from_digit(digit.to_u32().unwrap_or(0), 10).unwrap_or('0');
        frac_digits.push(d);
        if d != '0' {
            hit_nonzero = true;
        }
        if hit_nonzero {
            sig_count += 1;
        }
    }

    // If we never hit a non-zero digit the fractional part is zero — return the
    // integer part alone. This is unreachable when remainder != 0 because the loop
    // would continue indefinitely until sig_count reaches remaining_sig (each zero
    // digit does not increment sig_count, so the loop keeps going until it finds a
    // non-zero digit or remainder reaches zero). Kept as a safety net.
    let frac_trimmed = frac_digits.trim_end_matches('0');
    if frac_trimmed.is_empty() {
        int_str
    } else {
        format!("{int_str}.{frac_trimmed}")
    }
}

/// Truncate `digits` (an integer string) to at most `max_sig` significant digits,
/// removing leading zeros first. Trailing zeros are always preserved because they
/// encode magnitude in an integer (1500 != 15, and the trailing zeros in a
/// truncated value like 1e17 -> "10000000000000000" are significant).
fn truncate_sig_digits(digits: &str, max_sig: usize) -> String {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return "0".to_string();
    }
    if trimmed.len() <= max_sig {
        return trimmed.to_string();
    }
    // Truncation occurred: trailing zeros in the kept portion are significant
    // (they encode magnitude). Do NOT strip them.
    trimmed[..max_sig].to_string()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use serde::Deserialize;

    use super::*;

    // ---- Fixture metadata validation ----
    //
    // The shared fixture file lives in both repos and carries metadata the frontend
    // pins (contract version, price unit, gas token). The server only needs to verify
    // that the metadata it serializes matches the fixture's expectations. The
    // frontend-only validation cases (missing_decimals, decimal_scale_out_of_range,
    // duplicate_address, gas_token_mismatch) are not re-implemented here.

    const UNIT_CONTRACT_FIXTURE: &str =
        include_str!("../../tests/fixtures/v1-prices-unit-contract-v1.json");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UnitContractFixture {
        contract_version: String,
        price_unit: String,
    }

    #[test]
    fn test_fixture_metadata_matches_server_consts() {
        let fixture: UnitContractFixture = serde_json::from_str(UNIT_CONTRACT_FIXTURE)
            .expect("unit contract fixture must be valid JSON");
        assert_eq!(fixture.contract_version, PRICE_UNIT_CONTRACT_V1);
        assert_eq!(fixture.price_unit, RAW_TOKEN_UNITS_PER_RAW_GAS_UNIT);
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

    // ---- price_to_f64 validation ----

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

    // ---- price_to_decimal_string representation ----

    #[test]
    fn decimal_string_exact_integer() {
        let n = BigUint::from(1500u64);
        let d = BigUint::from(1u8);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "1500");
    }

    #[test]
    fn decimal_string_small_fraction() {
        let n = BigUint::from(3u64);
        let d = BigUint::from(1_000_000_000u64);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "0.000000003");
    }

    #[test]
    fn decimal_string_one_half() {
        let n = BigUint::from(1u64);
        let d = BigUint::from(2u64);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "0.5");
    }

    #[test]
    fn decimal_string_trailing_zeros_trimmed() {
        let n = BigUint::from(5u64);
        let d = BigUint::from(10u64);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "0.5");
    }

    #[test]
    fn decimal_string_repeating_truncated() {
        // 1/3 = 0.333... truncated to 17 sig digits = 0.33333333333333333
        let n = BigUint::from(1u64);
        let d = BigUint::from(3u64);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "0.33333333333333333");
    }

    #[test]
    fn decimal_string_zero_numerator_returns_none() {
        assert!(price_to_decimal_string(&BigUint::from(0u8), &BigUint::from(1u8)).is_none());
    }

    #[test]
    fn decimal_string_zero_denominator_returns_none() {
        assert!(price_to_decimal_string(&BigUint::from(1u8), &BigUint::from(0u8)).is_none());
    }

    #[test]
    fn decimal_string_no_scientific_notation() {
        // A value that f64 would render as 3e-9 must be a plain decimal string.
        let n = BigUint::from(3u64);
        let d = BigUint::from(1_000_000_000u64);
        let s = price_to_decimal_string(&n, &d).unwrap();
        assert!(!s.contains('e') && !s.contains('E'));
    }

    #[test]
    fn decimal_string_serializes_as_json_string() {
        let n = BigUint::from(3u64);
        let d = BigUint::from(1_000_000_000u64);
        let price = price_to_decimal_string(&n, &d).unwrap();
        let entry = TokenPriceEntry {
            token: Address::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            price,
        };
        let serialized = serde_json::to_value(entry).unwrap();
        assert!(serialized["price"].is_string(), "price must serialize as a JSON string");
        assert_eq!(serialized["price"].as_str().unwrap(), "0.000000003");
    }

    #[test]
    fn decimal_string_max_significant_digits() {
        // A large integer that exceeds 17 significant digits gets truncated.
        let n = BigUint::from(123_456_789_012_345_678_901u128);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        // 12345678901234567 (17 digits) -- trailing zeros already absent
        assert_eq!(result, "12345678901234567");
    }

    #[test]
    fn decimal_string_large_integer_with_fraction() {
        // 1500 + 1/3 -> integer part has 4 sig digits, 13 more from fraction.
        let n = BigUint::from(4501u64);
        let d = BigUint::from(3u64);
        let result = price_to_decimal_string(&n, &d).unwrap();
        // 1500.3333333333333 (4 + 13 = 17 sig digits)
        assert_eq!(result, "1500.3333333333333");
    }

    // ---- F1 regression: trailing zeros after truncation ----

    #[test]
    fn decimal_string_truncate_preserves_trailing_zeros() {
        // 1e17 / 1 = 100000000000000000 (18 digits). Truncating to 17 sig digits
        // yields "10000000000000000" — the trailing zero is significant (magnitude).
        let n = BigUint::from(10u64).pow(17);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "10000000000000000");
    }

    #[test]
    fn decimal_string_truncate_with_internal_zeros() {
        // 1.2e17 / 1 = 120000000000000000 (18 digits). Truncating to 17 sig digits
        // yields "12000000000000000" — trailing zeros are significant.
        let n = BigUint::from(12u64) * BigUint::from(10u64).pow(16);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "12000000000000000");
    }

    // ---- F2 regression: non-zero sub-unit values must not serialize as "0" ----

    #[test]
    fn decimal_string_small_nonzero_not_zero() {
        // 1 / (3e18) — a value that f64 can represent (~3.33e-19) but the old
        // algorithm serialized as "0" because 17 leading fractional zeros
        // exhausted the iteration budget.
        let n = BigUint::from(1u64);
        let d = BigUint::from(3u64) * BigUint::from(10u64).pow(18);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_ne!(result, "0", "non-zero value must not serialize as 0");
        assert!(result.starts_with("0."), "sub-unit value must start with 0.");
        // The first non-zero digit is 3 at position 19 after the decimal point.
        // 17 significant digits of 1/3 = 333... so the result is
        // 0.00000000000000000033333333333333333
        assert_eq!(result, "0.00000000000000000033333333333333333");
    }

    // ---- F3 regression: leading fractional zeros don't consume sig-digit budget ----

    #[test]
    fn decimal_string_leading_frac_zeros_full_precision() {
        // 1/700 = 0.0014285714285714285...
        // Two leading zeros after decimal point should NOT consume budget.
        // Result should have 17 significant digits, not 15.
        let n = BigUint::from(1u64);
        let d = BigUint::from(700u64);
        let result = price_to_decimal_string(&n, &d).unwrap();
        // 17 significant digits: 14285714285714285 (then trailing zeros trimmed)
        assert_eq!(result, "0.0014285714285714285");
        // Verify we got 17 sig digits by counting non-zero/non-leading-zero digits
        let frac_part = result.split('.').nth(1).unwrap();
        let sig_digits = frac_part.trim_start_matches('0');
        // 14285714285714285 = 17 digits
        assert_eq!(sig_digits.len(), 17);
    }

    #[test]
    fn decimal_string_small_repeating_fraction() {
        // 1 / (3e9) = 0.00000000033333333...
        // Nine leading zeros should not consume budget; 17 sig digits of 3s.
        let n = BigUint::from(1u64);
        let d = BigUint::from(3u64) * BigUint::from(10u64).pow(9);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "0.00000000033333333333333333");
        // Verify 17 significant digits (all 3s)
        let frac_part = result.split('.').nth(1).unwrap();
        let sig_digits = frac_part.trim_start_matches('0');
        assert_eq!(sig_digits.len(), 17);
    }

    // ---- F4 regression: f64 gate removed, BigUint range guard in effect ----

    #[test]
    fn decimal_string_large_value_outside_f64_range() {
        // 10^100 / 1 — far outside f64 range (f64 max ~1.8e308). The old f64
        // gatekeeper would reject this. The new BigUint-based guard accepts it
        // because 101 digits < MAX_BIGUINT_DIGITS (400).
        let n = BigUint::from(10u8).pow(100);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d);
        assert!(result.is_some(), "value within BigUint range must not be rejected");
        let s = result.unwrap();
        // Truncated to 17 sig digits: "1" followed by 16 zeros = 17 chars.
        assert_eq!(s, "10000000000000000");
    }

    #[test]
    fn decimal_string_pathological_size_rejected() {
        // 10^400 has 401 digits — exceeds MAX_BIGUINT_DIGITS.
        let n = BigUint::from(10u8).pow(400);
        let d = BigUint::from(1u8);
        assert!(price_to_decimal_string(&n, &d).is_none());
    }

    #[test]
    fn decimal_string_pathological_denominator_rejected() {
        // 1 / 10^400 — denominator has 401 digits, exceeds MAX_BIGUINT_DIGITS.
        let n = BigUint::from(1u8);
        let d = BigUint::from(10u8).pow(400);
        assert!(price_to_decimal_string(&n, &d).is_none());
    }
}
