//! Types and helpers for the GET /v1/prices endpoint.

use std::fmt;

use fynd_core::types::ComponentId;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;
use utoipa::{IntoParams, ToSchema};

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
    /// Token gas prices relative to the native gas token, sorted by token address.
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
    /// Raw target-token units divided by raw gas-token units, serialized as a plain decimal
    /// string with up to 17 significant digits (no scientific notation).
    ///
    /// Intended for display and analytics only. Consumers must normalize both tokens'
    /// decimals before using it, and should parse it with a decimal-aware parser
    /// (BigDecimal, BigNumber, etc.) — the string format avoids `f64` rendering
    /// inconsistencies across languages.
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

/// Maximum bit length of numerator or denominator before a price is considered too large to
/// process safely. 1328 bits ≈ 400 decimal digits; larger operands would make the long
/// division and its output pathologically large.
const MAX_PRICE_OPERAND_BITS: u64 = 1328;

/// Convert a `tycho_core::Price { numerator, denominator }` to a decimal string.
///
/// Performs exact integer long-division and emits up to [`PRICE_DECIMAL_PRECISION`]
/// significant digits in plain decimal notation (no scientific notation, no trailing `.0`).
/// Digits beyond the precision budget are truncated (not rounded); magnitude is always
/// preserved.
///
/// Returns `None` when the numerator or denominator is zero (defensive: `Price::new`
/// panics on zeroes, but struct literals can still produce them) or when either operand
/// exceeds [`MAX_PRICE_OPERAND_BITS`] bits, which guards against unbounded computation.
pub fn price_to_decimal_string(
    numerator: &num_bigint::BigUint,
    denominator: &num_bigint::BigUint,
) -> Option<String> {
    use num_traits::Zero;

    if denominator.is_zero() || numerator.is_zero() {
        return None;
    }
    if numerator.bits() > MAX_PRICE_OPERAND_BITS || denominator.bits() > MAX_PRICE_OPERAND_BITS {
        return None;
    }

    Some(biguint_division_to_decimal_string(numerator, denominator, PRICE_DECIMAL_PRECISION))
}

/// Perform exact integer long-division and format the result as a plain decimal string
/// with at most `max_sig_digits` significant digits (trailing zeros removed from the
/// fractional part, no scientific notation).
///
/// For values below one, leading fractional zeros do not consume significant-digit budget:
/// for a value like 1/700 (0.00142...), the two leading zeros after the decimal point are
/// skipped, and the full 17 significant digits are produced. Once any non-zero digit
/// precedes them, fractional zeros are significant and do consume budget.
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

    // Long-division for the fractional part. Significant digits are tracked explicitly:
    // zeros before the first non-zero digit of a sub-unit value do not consume budget,
    // so values like 1/700 produce the full 17 significant digits and non-zero values
    // with many leading fractional zeros never serialize as "0". Once the integer part
    // (or an earlier fractional digit) is non-zero, every further digit is significant.
    let mut frac_digits = String::new();
    let mut rem = remainder;
    let ten = BigUint::from(10u8);
    let mut sig_count = 0usize;
    let mut hit_nonzero = int_sig > 0;

    while sig_count < remaining_sig {
        if rem.is_zero() {
            break;
        }
        rem *= &ten;
        let digit = (&rem / denominator)
            .to_u8()
            .expect("long-division digit is < 10 because rem < denominator before the multiply");
        rem = &rem % denominator;
        frac_digits.push(char::from(b'0' + digit));
        if digit != 0 {
            hit_nonzero = true;
        }
        if hit_nonzero {
            sig_count += 1;
        }
    }

    // All emitted fractional digits can be zeros when the integer part is non-zero and
    // the value's fraction lies below the remaining precision (e.g. (10^30 + 1) / 10^30):
    // the budget is spent on zeros that then trim away, leaving the integer part alone.
    let frac_trimmed = frac_digits.trim_end_matches('0');
    if frac_trimmed.is_empty() {
        int_str
    } else {
        format!("{int_str}.{frac_trimmed}")
    }
}

/// Truncate `digits` (an integer string) to at most `max_sig` significant digits while
/// preserving magnitude: digits beyond the budget are replaced with zeros, not dropped
/// (10^17 stays `"100000000000000000"`, and never becomes the 10x smaller
/// `"10000000000000000"`).
fn truncate_sig_digits(digits: &str, max_sig: usize) -> String {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return "0".to_string();
    }
    if trimmed.len() <= max_sig {
        return trimmed.to_string();
    }
    let mut truncated = trimmed[..max_sig].to_string();
    truncated.push_str(&"0".repeat(trimmed.len() - max_sig));
    truncated
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;

    use super::*;

    // ---- IncludeField parsing ----

    #[test]
    fn test_parse_include_empty() {
        assert_eq!(IncludeField::parse_include("").unwrap(), vec![]);
    }

    #[test]
    fn test_parse_include_depths() {
        let fields = IncludeField::parse_include("depths").unwrap();
        assert_eq!(fields, vec![IncludeField::Depths]);
    }

    #[test]
    fn test_parse_include_spot_prices() {
        let fields = IncludeField::parse_include("spot_prices").unwrap();
        assert_eq!(fields, vec![IncludeField::SpotPrices]);
    }

    #[test]
    fn test_parse_include_both() {
        let fields = IncludeField::parse_include("depths,spot_prices").unwrap();
        assert_eq!(fields, vec![IncludeField::Depths, IncludeField::SpotPrices]);
    }

    #[test]
    fn test_parse_include_with_whitespace() {
        let fields = IncludeField::parse_include(" depths , spot_prices ").unwrap();
        assert_eq!(fields, vec![IncludeField::Depths, IncludeField::SpotPrices]);
    }

    #[test]
    fn test_parse_include_unknown_rejects() {
        let err = IncludeField::parse_include("depths,foobar").unwrap_err();
        assert!(err.contains("foobar"));
    }

    // ---- price_to_decimal_string representation ----

    #[test]
    fn test_decimal_string_exact_integer() {
        let n = BigUint::from(1500u64);
        let d = BigUint::from(1u8);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "1500");
    }

    #[test]
    fn test_decimal_string_small_fraction() {
        let n = BigUint::from(3u64);
        let d = BigUint::from(1_000_000_000u64);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "0.000000003");
    }

    #[test]
    fn test_decimal_string_one_half() {
        let n = BigUint::from(1u64);
        let d = BigUint::from(2u64);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "0.5");
    }

    #[test]
    fn test_decimal_string_trailing_zeros_trimmed() {
        let n = BigUint::from(5u64);
        let d = BigUint::from(10u64);
        assert_eq!(price_to_decimal_string(&n, &d).unwrap(), "0.5");
    }

    #[test]
    fn test_decimal_string_repeating_truncated() {
        // 1/3 = 0.333... truncated to 17 sig digits = 0.33333333333333333
        let n = BigUint::from(1u64);
        let d = BigUint::from(3u64);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "0.33333333333333333");
    }

    // Zero operands are constructor-unreachable (tycho's `Price::new` panics on them) but
    // remain representable via struct literals, so the guards are exercised defensively.
    #[test]
    fn test_decimal_string_zero_numerator_returns_none() {
        assert!(price_to_decimal_string(&BigUint::from(0u8), &BigUint::from(1u8)).is_none());
    }

    #[test]
    fn test_decimal_string_zero_denominator_returns_none() {
        assert!(price_to_decimal_string(&BigUint::from(1u8), &BigUint::from(0u8)).is_none());
    }

    #[test]
    fn test_decimal_string_no_scientific_notation() {
        // 10^21 is rendered in scientific notation by both Rust's and JavaScript's f64
        // formatters; the decimal string must stay plain and keep its magnitude.
        let n = BigUint::from(10u8).pow(21);
        let d = BigUint::from(1u8);
        let s = price_to_decimal_string(&n, &d).unwrap();
        assert!(!s.contains('e') && !s.contains('E'));
        assert_eq!(s, format!("1{}", "0".repeat(21)));
    }

    #[test]
    fn test_decimal_string_max_significant_digits() {
        // A 21-digit integer keeps its magnitude: digits beyond the 17-significant-digit
        // budget are zeroed, not dropped.
        let n = BigUint::from(123_456_789_012_345_678_901u128);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "123456789012345670000");
    }

    #[test]
    fn test_decimal_string_large_integer_with_fraction() {
        // 1500 + 1/3 -> integer part has 4 sig digits, 13 more from fraction.
        let n = BigUint::from(4501u64);
        let d = BigUint::from(3u64);
        let result = price_to_decimal_string(&n, &d).unwrap();
        // 1500.3333333333333 (4 + 13 = 17 sig digits)
        assert_eq!(result, "1500.3333333333333");
    }

    // ---- Magnitude preservation on truncation ----

    #[test]
    fn test_decimal_string_truncate_preserves_trailing_zeros() {
        // 10^17 has one significant digit; truncating to 17 must not change the value.
        let n = BigUint::from(10u64).pow(17);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "100000000000000000");
    }

    #[test]
    fn test_decimal_string_truncate_with_internal_zeros() {
        // 1.2 * 10^17 (18 digits) keeps all 18 digits; only precision beyond 17
        // significant digits may be zeroed.
        let n = BigUint::from(12u64) * BigUint::from(10u64).pow(16);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "120000000000000000");
    }

    #[test]
    fn test_decimal_string_int_part_exactly_17_digits_with_fraction() {
        // (3 * 10^16 + 1) / 3 = 10^16 + 1/3: the 17-digit integer part exhausts the
        // budget, so the fraction is dropped and the integer is unchanged.
        let n = BigUint::from(3u64) * BigUint::from(10u64).pow(16) + BigUint::from(1u8);
        let d = BigUint::from(3u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "10000000000000000");
    }

    #[test]
    fn test_decimal_string_int_part_18_digits_with_fraction() {
        // (3 * 10^17 + 1) / 3 = 10^17 + 1/3: 18-digit integer part, fraction dropped,
        // magnitude preserved.
        let n = BigUint::from(3u64) * BigUint::from(10u64).pow(17) + BigUint::from(1u8);
        let d = BigUint::from(3u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "100000000000000000");
    }

    #[test]
    fn test_decimal_string_large_value_within_operand_bound() {
        // 10^100 is far outside f64 range but well within the operand bound; its
        // single significant digit and full magnitude must both survive.
        let n = BigUint::from(10u8).pow(100);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, format!("1{}", "0".repeat(100)));
    }

    // ---- Sub-unit values ----

    #[test]
    fn test_decimal_string_small_nonzero_not_zero() {
        // 1 / (3e18) ~ 3.33e-19: leading fractional zeros must not exhaust the budget
        // and collapse the value to "0".
        let n = BigUint::from(1u64);
        let d = BigUint::from(3u64) * BigUint::from(10u64).pow(18);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "0.00000000000000000033333333333333333");
    }

    #[test]
    fn test_decimal_string_leading_frac_zeros_full_precision() {
        // 1/700 = 0.0014285714285714285...: the two leading zeros after the decimal
        // point do not consume budget, so all 17 significant digits are produced.
        let n = BigUint::from(1u64);
        let d = BigUint::from(700u64);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "0.0014285714285714285");
    }

    #[test]
    fn test_decimal_string_small_repeating_fraction() {
        // 1 / (3e9) = 0.00000000033333333...: nine leading zeros, then 17 sig digits.
        let n = BigUint::from(1u64);
        let d = BigUint::from(3u64) * BigUint::from(10u64).pow(9);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "0.00000000033333333333333333");
    }

    #[test]
    fn test_decimal_string_fractional_zeros_after_nonzero_int_are_significant() {
        // (10^30 + 1) / 10^30 = 1.000...001 with the 1 beyond the precision budget:
        // fractional zeros after a non-zero integer part consume budget, so the result
        // is "1", not a 31-significant-digit string.
        let n = BigUint::from(10u64).pow(30) + BigUint::from(1u8);
        let d = BigUint::from(10u64).pow(30);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, "1");
    }

    #[test]
    fn test_decimal_string_sig_digit_budget_invariant() {
        // The output never carries more than PRICE_DECIMAL_PRECISION significant digits,
        // for values above and below one, across truncation and zero-skipping paths.
        let cases = [
            (BigUint::from(1u8), BigUint::from(3u8)),
            (BigUint::from(4501u64), BigUint::from(3u8)),
            (BigUint::from(10u64).pow(17), BigUint::from(1u8)),
            (BigUint::from(123_456_789_012_345_678_901u128), BigUint::from(1u8)),
            (BigUint::from(10u8).pow(100), BigUint::from(1u8)),
            (BigUint::from(10u64).pow(30) + BigUint::from(1u8), BigUint::from(10u64).pow(30)),
            (BigUint::from(10u8).pow(399) + BigUint::from(1u8), BigUint::from(10u8).pow(399)),
            (BigUint::from(1u8), BigUint::from(700u64)),
            (BigUint::from(1u8), BigUint::from(3u64) * BigUint::from(10u64).pow(18)),
            (BigUint::from_str("999999999999999999").unwrap(), BigUint::from(7u8)),
        ];
        for (numerator, denominator) in cases {
            let price = price_to_decimal_string(&numerator, &denominator).unwrap();
            let digits: String = price
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            let significant = digits
                .trim_start_matches('0')
                .trim_end_matches('0')
                .len();
            assert!(
                significant <= PRICE_DECIMAL_PRECISION,
                "{numerator}/{denominator} -> {price} has {significant} significant digits"
            );
        }
    }

    // ---- Operand size guard ----

    #[test]
    fn test_decimal_string_max_operand_accepted() {
        // 10^399 (400 digits, 1326 bits) is within the operand bound and keeps its
        // magnitude through truncation.
        let n = BigUint::from(10u8).pow(399);
        let d = BigUint::from(1u8);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, format!("1{}", "0".repeat(399)));
    }

    #[test]
    fn test_decimal_string_max_denominator_accepted() {
        // 1 / 10^399 exercises the longest leading-zero run the long division can
        // produce: 398 fractional zeros before the single significant digit.
        let n = BigUint::from(1u8);
        let d = BigUint::from(10u8).pow(399);
        let result = price_to_decimal_string(&n, &d).unwrap();
        assert_eq!(result, format!("0.{}1", "0".repeat(398)));
    }

    #[test]
    fn test_decimal_string_pathological_size_rejected() {
        // 10^400 needs 1329 bits — beyond MAX_PRICE_OPERAND_BITS.
        let n = BigUint::from(10u8).pow(400);
        let d = BigUint::from(1u8);
        assert!(price_to_decimal_string(&n, &d).is_none());
    }

    #[test]
    fn test_decimal_string_pathological_denominator_rejected() {
        let n = BigUint::from(1u8);
        let d = BigUint::from(10u8).pow(400);
        assert!(price_to_decimal_string(&n, &d).is_none());
    }
}
