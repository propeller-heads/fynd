//! Schema validation for parsed quote records.
//!
//! Validates [`QuoteRecord`] instances against semantic constraints that go
//! beyond JSON deserialization: supported chain IDs, numeric format of amount
//! strings, block field ranges, swap split bounds, and address-like hex
//! formats. Collects all violations rather than short-circuiting on the first.

use crate::QuoteRecord;

/// Supported chain IDs in v1 (Ethereum mainnet and Base).
const SUPPORTED_CHAIN_IDS: &[u64] = &[1, 8453];

/// A single schema violation found during validation.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaViolation {
    /// Dot-path to the offending field (e.g. `route.swaps[0].split`).
    pub field: String,
    /// What constraint was violated.
    pub kind: ViolationKind,
}

/// Categorises the type of schema violation.
#[derive(Debug, Clone, PartialEq)]
pub enum ViolationKind {
    /// A string field is empty when it must have content.
    EmptyString,
    /// A string that should represent a non-negative integer is malformed.
    InvalidNumericString { value: String },
    /// A value is outside its allowed range.
    OutOfRange { value: String, constraint: String },
    /// A chain ID is not in the supported set.
    UnsupportedChainId { chain_id: u64 },
    /// A hex string does not match the expected format.
    InvalidHexFormat { value: String },
}

impl std::fmt::Display for SchemaViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ViolationKind::EmptyString => {
                write!(f, "{}: must not be empty", self.field)
            }
            ViolationKind::InvalidNumericString { value } => {
                write!(f, "{}: expected non-negative integer string, got '{value}'", self.field)
            }
            ViolationKind::OutOfRange { value, constraint, .. } => {
                write!(f, "{}: value {value} is out of range ({constraint})", self.field)
            }
            ViolationKind::UnsupportedChainId { chain_id } => {
                write!(
                    f,
                    "{}: chain_id {chain_id} is not supported (expected one of {SUPPORTED_CHAIN_IDS:?})",
                    self.field
                )
            }
            ViolationKind::InvalidHexFormat { value } => {
                write!(f, "{}: invalid hex format '{value}'", self.field)
            }
        }
    }
}

/// Validate a parsed [`QuoteRecord`] against the dataset schema.
///
/// Returns `Ok(())` when the record passes all checks, or `Err` with every
/// violation found. The validator never short-circuits — it collects all
/// problems so the caller can report them in one pass.
pub fn validate_quote_record(record: &QuoteRecord) -> Result<(), Vec<SchemaViolation>> {
    let mut violations = Vec::new();

    validate_top_level(record, &mut violations);
    validate_block(&record.block, &mut violations);
    validate_route(record, &mut violations);

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

// ---- internal helpers ----

fn validate_top_level(record: &QuoteRecord, violations: &mut Vec<SchemaViolation>) {
    if record.quote_id.is_empty() {
        violations.push(SchemaViolation {
            field: "quote_id".to_owned(),
            kind: ViolationKind::EmptyString,
        });
    }

    if !SUPPORTED_CHAIN_IDS.contains(&record.chain_id) {
        violations.push(SchemaViolation {
            field: "chain_id".to_owned(),
            kind: ViolationKind::UnsupportedChainId { chain_id: record.chain_id },
        });
    }

    check_numeric_string("amount_in", &record.amount_in, violations);
    check_numeric_string("amount_out", &record.amount_out, violations);
    check_numeric_string("gas_estimate", &record.gas_estimate, violations);
}

fn validate_block(block: &crate::BlockRecord, violations: &mut Vec<SchemaViolation>) {
    if block.number == 0 {
        violations.push(SchemaViolation {
            field: "block.number".to_owned(),
            kind: ViolationKind::OutOfRange {
                value: "0".to_owned(),
                constraint: "must be > 0".to_owned(),
            },
        });
    }

    if block.timestamp == 0 {
        violations.push(SchemaViolation {
            field: "block.timestamp".to_owned(),
            kind: ViolationKind::OutOfRange {
                value: "0".to_owned(),
                constraint: "must be > 0".to_owned(),
            },
        });
    }

    check_hex_string("block.hash", &block.hash, violations);
}

fn validate_route(record: &QuoteRecord, violations: &mut Vec<SchemaViolation>) {
    if record.route.swaps.is_empty() {
        violations.push(SchemaViolation {
            field: "route.swaps".to_owned(),
            kind: ViolationKind::OutOfRange {
                value: "0".to_owned(),
                constraint: "must contain at least one swap".to_owned(),
            },
        });
        return;
    }

    for (i, swap) in record.route.swaps.iter().enumerate() {
        let prefix = format!("route.swaps[{i}]");
        validate_swap(swap, &prefix, violations);
    }
}

fn validate_swap(swap: &crate::SwapRecord, prefix: &str, violations: &mut Vec<SchemaViolation>) {
    if swap.component_id.is_empty() {
        violations.push(SchemaViolation {
            field: format!("{prefix}.component_id"),
            kind: ViolationKind::EmptyString,
        });
    }

    if swap.protocol.is_empty() {
        violations.push(SchemaViolation {
            field: format!("{prefix}.protocol"),
            kind: ViolationKind::EmptyString,
        });
    }

    check_hex_string(&format!("{prefix}.token_in"), &swap.token_in, violations);
    check_hex_string(&format!("{prefix}.token_out"), &swap.token_out, violations);

    check_numeric_string(&format!("{prefix}.amount_in"), &swap.amount_in, violations);
    check_numeric_string(&format!("{prefix}.amount_out"), &swap.amount_out, violations);
    check_numeric_string(&format!("{prefix}.gas_estimate"), &swap.gas_estimate, violations);

    if !(0.0..=1.0).contains(&swap.split) {
        violations.push(SchemaViolation {
            field: format!("{prefix}.split"),
            kind: ViolationKind::OutOfRange {
                value: swap.split.to_string(),
                constraint: "must be in [0.0, 1.0]".to_owned(),
            },
        });
    }
}

/// Check that a string represents a non-negative integer (decimal digits only).
fn check_numeric_string(field: &str, value: &str, violations: &mut Vec<SchemaViolation>) {
    if value.is_empty() ||
        !value
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        violations.push(SchemaViolation {
            field: field.to_owned(),
            kind: ViolationKind::InvalidNumericString { value: value.to_owned() },
        });
    }
}

/// Check that a string looks like a hex value (starts with `0x`, followed by
/// at least one hex digit).
fn check_hex_string(field: &str, value: &str, violations: &mut Vec<SchemaViolation>) {
    let valid = value.starts_with("0x") &&
        value.len() > 2 &&
        value[2..]
            .chars()
            .all(|c| c.is_ascii_hexdigit());

    if !valid {
        violations.push(SchemaViolation {
            field: field.to_owned(),
            kind: ViolationKind::InvalidHexFormat { value: value.to_owned() },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockRecord, QuoteRecord, RouteRecord, SwapRecord};

    /// Build a valid Ethereum single-hop quote record.
    fn valid_record() -> QuoteRecord {
        QuoteRecord {
            quote_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned(),
            chain_id: 1,
            block: BlockRecord {
                number: 21_000_000,
                hash: "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                    .to_owned(),
                timestamp: 1_730_000_000,
            },
            route: RouteRecord {
                swaps: vec![SwapRecord {
                    component_id: "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc".to_owned(),
                    protocol: "uniswap_v2".to_owned(),
                    token_in: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_owned(),
                    token_out: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_owned(),
                    amount_in: "1000000000000000000".to_owned(),
                    amount_out: "3500000000".to_owned(),
                    gas_estimate: "150000".to_owned(),
                    split: 1.0,
                }],
            },
            amount_in: "1000000000000000000".to_owned(),
            amount_out: "3500000000".to_owned(),
            gas_estimate: "150000".to_owned(),
        }
    }

    /// Build a valid Base multi-hop quote record.
    fn valid_base_record() -> QuoteRecord {
        QuoteRecord {
            quote_id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_owned(),
            chain_id: 8453,
            block: BlockRecord {
                number: 5_000_000,
                hash: "0x1111111111111111111111111111111111111111111111111111111111111111"
                    .to_owned(),
                timestamp: 1_700_000_000,
            },
            route: RouteRecord {
                swaps: vec![
                    SwapRecord {
                        component_id: "0xaaaa1111bbbb2222cccc3333dddd4444eeee5555".to_owned(),
                        protocol: "uniswap_v3".to_owned(),
                        token_in: "0x4200000000000000000000000000000000000006".to_owned(),
                        token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_owned(),
                        amount_in: "500".to_owned(),
                        amount_out: "400".to_owned(),
                        gas_estimate: "80000".to_owned(),
                        split: 1.0,
                    },
                    SwapRecord {
                        component_id: "0xbbbb2222cccc3333dddd4444eeee5555ffff6666".to_owned(),
                        protocol: "uniswap_v2".to_owned(),
                        token_in: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_owned(),
                        token_out: "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb".to_owned(),
                        amount_in: "400".to_owned(),
                        amount_out: "350".to_owned(),
                        gas_estimate: "70000".to_owned(),
                        split: 1.0,
                    },
                ],
            },
            amount_in: "500".to_owned(),
            amount_out: "350".to_owned(),
            gas_estimate: "150000".to_owned(),
        }
    }

    // ---- Acceptance of valid records ----

    #[test]
    fn accept_valid_ethereum_record() {
        let record = valid_record();
        assert!(
            validate_quote_record(&record).is_ok(),
            "valid Ethereum record should pass schema validation"
        );
    }

    #[test]
    fn accept_valid_base_multi_hop_record() {
        let record = valid_base_record();
        assert!(
            validate_quote_record(&record).is_ok(),
            "valid Base multi-hop record should pass schema validation"
        );
    }

    #[test]
    fn accept_zero_split_value() {
        let mut record = valid_record();
        record.route.swaps[0].split = 0.0;
        assert!(validate_quote_record(&record).is_ok(), "split=0.0 is a valid boundary value");
    }

    // ---- Rejection: missing/empty required fields ----

    #[test]
    fn reject_empty_quote_id() {
        let mut record = valid_record();
        record.quote_id = String::new();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "quote_id" && v.kind == ViolationKind::EmptyString));
    }

    #[test]
    fn reject_empty_component_id() {
        let mut record = valid_record();
        record.route.swaps[0].component_id = String::new();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[0].component_id" &&
                v.kind == ViolationKind::EmptyString));
    }

    #[test]
    fn reject_empty_protocol() {
        let mut record = valid_record();
        record.route.swaps[0].protocol = String::new();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[0].protocol" && v.kind == ViolationKind::EmptyString));
    }

    #[test]
    fn reject_empty_swaps_array() {
        let mut record = valid_record();
        record.route.swaps.clear();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps"));
    }

    // ---- Rejection: invalid field types / formats ----

    #[test]
    fn reject_unsupported_chain_id() {
        let mut record = valid_record();
        record.chain_id = 42161; // Arbitrum — deferred to v2
        let errs = validate_quote_record(&record).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(matches!(&errs[0].kind, ViolationKind::UnsupportedChainId { chain_id: 42161 }));
    }

    #[test]
    fn reject_non_numeric_amount_in() {
        let mut record = valid_record();
        record.amount_in = "not_a_number".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "amount_in" &&
                matches!(&v.kind, ViolationKind::InvalidNumericString { .. })));
    }

    #[test]
    fn reject_negative_amount_out() {
        let mut record = valid_record();
        record.amount_out = "-100".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "amount_out" &&
                matches!(&v.kind, ViolationKind::InvalidNumericString { .. })));
    }

    #[test]
    fn reject_empty_gas_estimate_string() {
        let mut record = valid_record();
        record.gas_estimate = String::new();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "gas_estimate" &&
                matches!(&v.kind, ViolationKind::InvalidNumericString { .. })));
    }

    #[test]
    fn reject_non_numeric_swap_amount() {
        let mut record = valid_record();
        record.route.swaps[0].amount_in = "abc".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[0].amount_in" &&
                matches!(&v.kind, ViolationKind::InvalidNumericString { .. })));
    }

    // ---- Rejection: out-of-range values ----

    #[test]
    fn reject_block_number_zero() {
        let mut record = valid_record();
        record.block.number = 0;
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "block.number" &&
                matches!(&v.kind, ViolationKind::OutOfRange { .. })));
    }

    #[test]
    fn reject_block_timestamp_zero() {
        let mut record = valid_record();
        record.block.timestamp = 0;
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "block.timestamp" &&
                matches!(&v.kind, ViolationKind::OutOfRange { .. })));
    }

    #[test]
    fn reject_split_above_one() {
        let mut record = valid_record();
        record.route.swaps[0].split = 1.5;
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[0].split" &&
                matches!(&v.kind, ViolationKind::OutOfRange { .. })));
    }

    #[test]
    fn reject_negative_split() {
        let mut record = valid_record();
        record.route.swaps[0].split = -0.1;
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[0].split" &&
                matches!(&v.kind, ViolationKind::OutOfRange { .. })));
    }

    #[test]
    fn reject_invalid_block_hash_format() {
        let mut record = valid_record();
        record.block.hash = "not-a-hex-hash".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "block.hash" &&
                matches!(&v.kind, ViolationKind::InvalidHexFormat { .. })));
    }

    #[test]
    fn reject_block_hash_missing_0x_prefix() {
        let mut record = valid_record();
        record.block.hash =
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "block.hash" &&
                matches!(&v.kind, ViolationKind::InvalidHexFormat { .. })));
    }

    #[test]
    fn reject_invalid_token_address_format() {
        let mut record = valid_record();
        record.route.swaps[0].token_in = "not-an-address".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[0].token_in" &&
                matches!(&v.kind, ViolationKind::InvalidHexFormat { .. })));
    }

    // ---- Multiple violations collected ----

    #[test]
    fn collects_multiple_violations() {
        let mut record = valid_record();
        record.quote_id = String::new();
        record.chain_id = 999;
        record.block.number = 0;
        record.amount_in = "bad".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs.len() >= 4, "should collect at least 4 violations, got {}", errs.len());
    }

    #[test]
    fn second_hop_violations_reference_correct_index() {
        let mut record = valid_base_record();
        record.route.swaps[1].split = 2.0;
        record.route.swaps[1].amount_out = "xyz".to_owned();
        let errs = validate_quote_record(&record).unwrap_err();
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[1].split"));
        assert!(errs
            .iter()
            .any(|v| v.field == "route.swaps[1].amount_out"));
    }

    // ---- Display formatting ----

    #[test]
    fn violation_display_includes_field_and_detail() {
        let violation = SchemaViolation {
            field: "chain_id".to_owned(),
            kind: ViolationKind::UnsupportedChainId { chain_id: 42161 },
        };
        let msg = violation.to_string();
        assert!(msg.contains("chain_id"));
        assert!(msg.contains("42161"));
    }
}
