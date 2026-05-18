//! Quote record parsing for the slippage feature dataset.
//!
//! Deserializes raw Fynd quote data from JSON into typed structs suitable for
//! offline feature extraction. The historical source format mirrors the Fynd
//! RPC response enriched with `chain_id` (since Fynd instances are
//! chain-specific, the chain identifier is attached at export time).

use serde::{Deserialize, Serialize};

/// Errors that can occur when parsing a quote record.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    #[error("missing required field: {field}")]
    MissingField { field: String },

    #[error("invalid field value for '{field}': {reason}")]
    InvalidField { field: String, reason: String },
}

/// A parsed quote record ready for feature extraction.
///
/// Captures the minimum data needed from a historical Fynd quote to compute
/// slippage-decay features: the quote identity, block context, and route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteRecord {
    pub quote_id: String,
    pub chain_id: u64,
    pub block: BlockRecord,
    pub route: RouteRecord,
    pub amount_in: String,
    pub amount_out: String,
    pub gas_estimate: String,
}

impl QuoteRecord {
    /// Block number at which this quote was computed.
    pub fn block_number(&self) -> u64 {
        self.block.number
    }
}

/// Block information at the time a quote was computed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockRecord {
    pub number: u64,
    pub hash: String,
    pub timestamp: u64,
}

/// An ordered sequence of swap hops that together form a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRecord {
    pub swaps: Vec<SwapRecord>,
}

impl RouteRecord {
    /// Number of hops in this route.
    pub fn hop_count(&self) -> usize {
        self.swaps.len()
    }
}

/// A single swap hop within a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwapRecord {
    pub component_id: String,
    pub protocol: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
    pub amount_out: String,
    pub gas_estimate: String,
    pub split: f64,
}

/// Intermediate raw JSON shape that maps 1:1 to the historical export format.
///
/// The historical format enriches the Fynd `OrderQuote` response with a
/// top-level `chain_id` field.
#[derive(Deserialize)]
struct RawQuoteRecord {
    quote_id: Option<String>,
    chain_id: Option<u64>,
    block: Option<RawBlockInfo>,
    route: Option<RawRoute>,
    amount_in: Option<String>,
    amount_out: Option<String>,
    gas_estimate: Option<String>,
}

#[derive(Deserialize)]
struct RawBlockInfo {
    number: Option<u64>,
    hash: Option<String>,
    timestamp: Option<u64>,
}

#[derive(Deserialize)]
struct RawRoute {
    swaps: Option<Vec<RawSwap>>,
}

#[derive(Deserialize)]
struct RawSwap {
    component_id: Option<String>,
    protocol: Option<String>,
    token_in: Option<String>,
    token_out: Option<String>,
    amount_in: Option<String>,
    amount_out: Option<String>,
    gas_estimate: Option<String>,
    split: Option<f64>,
}

/// Parse a single JSON string into a [`QuoteRecord`].
///
/// Returns [`ParseError::InvalidJson`] if the input is not valid JSON,
/// [`ParseError::MissingField`] if a required field is absent or null.
pub fn parse_quote_record(json: &str) -> Result<QuoteRecord, ParseError> {
    let raw: RawQuoteRecord = serde_json::from_str(json)?;
    validate_and_convert(raw)
}

/// Parse a JSON array of quote records.
///
/// Returns the first error encountered; all preceding records are discarded.
pub fn parse_quote_records(json: &str) -> Result<Vec<QuoteRecord>, ParseError> {
    let raws: Vec<RawQuoteRecord> = serde_json::from_str(json)?;
    raws.into_iter()
        .map(validate_and_convert)
        .collect()
}

fn require_field<T>(value: Option<T>, field: &str) -> Result<T, ParseError> {
    value.ok_or_else(|| ParseError::MissingField { field: field.to_owned() })
}

fn validate_and_convert(raw: RawQuoteRecord) -> Result<QuoteRecord, ParseError> {
    let quote_id = require_field(raw.quote_id, "quote_id")?;
    let chain_id = require_field(raw.chain_id, "chain_id")?;
    let amount_in = require_field(raw.amount_in, "amount_in")?;
    let amount_out = require_field(raw.amount_out, "amount_out")?;
    let gas_estimate = require_field(raw.gas_estimate, "gas_estimate")?;

    let block = require_field(raw.block, "block")?;
    let block_number = require_field(block.number, "block.number")?;
    let block_hash = require_field(block.hash, "block.hash")?;
    let block_timestamp = require_field(block.timestamp, "block.timestamp")?;

    let raw_route = require_field(raw.route, "route")?;
    let raw_swaps = require_field(raw_route.swaps, "route.swaps")?;

    if raw_swaps.is_empty() {
        return Err(ParseError::InvalidField {
            field: "route.swaps".to_owned(),
            reason: "route must contain at least one swap".to_owned(),
        });
    }

    let mut swaps = Vec::with_capacity(raw_swaps.len());
    for (i, raw_swap) in raw_swaps.into_iter().enumerate() {
        swaps.push(convert_swap(raw_swap, i)?);
    }

    Ok(QuoteRecord {
        quote_id,
        chain_id,
        block: BlockRecord { number: block_number, hash: block_hash, timestamp: block_timestamp },
        route: RouteRecord { swaps },
        amount_in,
        amount_out,
        gas_estimate,
    })
}

fn convert_swap(raw: RawSwap, index: usize) -> Result<SwapRecord, ParseError> {
    let prefix = format!("route.swaps[{index}]");
    Ok(SwapRecord {
        component_id: require_field(raw.component_id, &format!("{prefix}.component_id"))?,
        protocol: require_field(raw.protocol, &format!("{prefix}.protocol"))?,
        token_in: require_field(raw.token_in, &format!("{prefix}.token_in"))?,
        token_out: require_field(raw.token_out, &format!("{prefix}.token_out"))?,
        amount_in: require_field(raw.amount_in, &format!("{prefix}.amount_in"))?,
        amount_out: require_field(raw.amount_out, &format!("{prefix}.amount_out"))?,
        gas_estimate: require_field(raw.gas_estimate, &format!("{prefix}.gas_estimate"))?,
        split: require_field(raw.split, &format!("{prefix}.split"))?,
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn valid_single_hop_json() -> String {
        serde_json::json!({
            "quote_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "chain_id": 1,
            "block": {
                "number": 21000000,
                "hash": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
                "timestamp": 1730000000
            },
            "route": {
                "swaps": [{
                    "component_id": "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc",
                    "protocol": "uniswap_v2",
                    "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                    "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                    "amount_in": "1000000000000000000",
                    "amount_out": "3500000000",
                    "gas_estimate": "150000",
                    "split": 1.0
                }]
            },
            "amount_in": "1000000000000000000",
            "amount_out": "3500000000",
            "gas_estimate": "150000"
        })
        .to_string()
    }

    fn valid_multi_hop_json() -> String {
        serde_json::json!({
            "quote_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "chain_id": 8453,
            "block": {
                "number": 5000000,
                "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                "timestamp": 1700000000
            },
            "route": {
                "swaps": [
                    {
                        "component_id": "0xpool1",
                        "protocol": "uniswap_v3",
                        "token_in": "0xTokenA",
                        "token_out": "0xTokenB",
                        "amount_in": "500",
                        "amount_out": "400",
                        "gas_estimate": "80000",
                        "split": 1.0
                    },
                    {
                        "component_id": "0xpool2",
                        "protocol": "uniswap_v2",
                        "token_in": "0xTokenB",
                        "token_out": "0xTokenC",
                        "amount_in": "400",
                        "amount_out": "350",
                        "gas_estimate": "70000",
                        "split": 1.0
                    }
                ]
            },
            "amount_in": "500",
            "amount_out": "350",
            "gas_estimate": "150000"
        })
        .to_string()
    }

    #[test]
    fn parse_valid_single_hop_quote() {
        let json = valid_single_hop_json();
        let record = parse_quote_record(&json).expect("should parse valid record");

        assert_eq!(record.quote_id, "f47ac10b-58cc-4372-a567-0e02b2c3d479");
        assert_eq!(record.chain_id, 1);
        assert_eq!(record.block_number(), 21_000_000);
        assert_eq!(record.block.number, 21_000_000);
        assert_eq!(
            record.block.hash,
            "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
        );
        assert_eq!(record.block.timestamp, 1_730_000_000);
        assert_eq!(record.amount_in, "1000000000000000000");
        assert_eq!(record.amount_out, "3500000000");
        assert_eq!(record.gas_estimate, "150000");

        assert_eq!(record.route.hop_count(), 1);
        let swap = &record.route.swaps[0];
        assert_eq!(swap.component_id, "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc");
        assert_eq!(swap.protocol, "uniswap_v2");
        assert_eq!(swap.token_in, "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert_eq!(swap.token_out, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        assert_eq!(swap.split, 1.0);
    }

    #[test]
    fn parse_valid_multi_hop_base_chain() {
        let json = valid_multi_hop_json();
        let record = parse_quote_record(&json).expect("should parse valid record");

        assert_eq!(record.chain_id, 8453);
        assert_eq!(record.block_number(), 5_000_000);
        assert_eq!(record.route.hop_count(), 2);
        assert_eq!(record.route.swaps[0].protocol, "uniswap_v3");
        assert_eq!(record.route.swaps[1].protocol, "uniswap_v2");
    }

    #[test]
    fn parse_batch_of_records() {
        let json = format!("[{}, {}]", valid_single_hop_json(), valid_multi_hop_json());
        let records = parse_quote_records(&json).expect("should parse array");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].chain_id, 1);
        assert_eq!(records[1].chain_id, 8453);
    }

    #[test]
    fn roundtrip_serialization() {
        let json = valid_single_hop_json();
        let record = parse_quote_record(&json).expect("should parse");
        let serialized = serde_json::to_string(&record).expect("should serialize");
        let roundtrip = parse_quote_record(&serialized).expect("should re-parse");
        assert_eq!(record, roundtrip);
    }

    // ---- Error cases ----

    #[test]
    fn error_on_invalid_json() {
        let result = parse_quote_record("not json at all");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson(_)));
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn error_on_empty_object() {
        let result = parse_quote_record("{}");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ParseError::MissingField { .. }));
        assert!(err.to_string().contains("quote_id"));
    }

    #[rstest]
    #[case::missing_quote_id("quote_id")]
    #[case::missing_chain_id("chain_id")]
    #[case::missing_block("block")]
    #[case::missing_route("route")]
    #[case::missing_amount_in("amount_in")]
    #[case::missing_amount_out("amount_out")]
    #[case::missing_gas_estimate("gas_estimate")]
    fn error_on_missing_top_level_field(#[case] field: &str) {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value
            .as_object_mut()
            .expect("object")
            .remove(field);
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(field), "error should mention '{field}': {err}");
    }

    #[rstest]
    #[case::missing_block_number("number")]
    #[case::missing_block_hash("hash")]
    #[case::missing_block_timestamp("timestamp")]
    fn error_on_missing_block_field(#[case] field: &str) {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value["block"]
            .as_object_mut()
            .expect("block object")
            .remove(field);
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let expected = format!("block.{field}");
        assert!(err.to_string().contains(&expected), "error should mention '{expected}': {err}");
    }

    #[rstest]
    #[case::missing_component_id("component_id")]
    #[case::missing_protocol("protocol")]
    #[case::missing_token_in("token_in")]
    #[case::missing_token_out("token_out")]
    #[case::missing_swap_amount_in("amount_in")]
    #[case::missing_swap_amount_out("amount_out")]
    #[case::missing_swap_gas_estimate("gas_estimate")]
    #[case::missing_split("split")]
    fn error_on_missing_swap_field(#[case] field: &str) {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value["route"]["swaps"][0]
            .as_object_mut()
            .expect("swap object")
            .remove(field);
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let expected = format!("route.swaps[0].{field}");
        assert!(err.to_string().contains(&expected), "error should mention '{expected}': {err}");
    }

    #[test]
    fn error_on_empty_swaps_array() {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value["route"]["swaps"] = serde_json::json!([]);
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ParseError::InvalidField { .. }));
        assert!(err
            .to_string()
            .contains("at least one swap"));
    }

    #[test]
    fn error_on_null_quote_id() {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value["quote_id"] = serde_json::Value::Null;
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("quote_id"));
    }

    #[test]
    fn error_on_wrong_type_for_block_number() {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value["block"]["number"] = serde_json::json!("not_a_number");
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ParseError::InvalidJson(_)));
    }

    #[test]
    fn error_on_wrong_type_for_chain_id() {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_single_hop_json()).expect("valid base");
        value["chain_id"] = serde_json::json!("ethereum");
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ParseError::InvalidJson(_)));
    }

    #[test]
    fn parse_preserves_second_hop_swap_index_in_error() {
        let mut value: serde_json::Value =
            serde_json::from_str(&valid_multi_hop_json()).expect("valid base");
        value["route"]["swaps"][1]
            .as_object_mut()
            .expect("swap object")
            .remove("protocol");
        let json = serde_json::to_string(&value).expect("re-serialize");

        let result = parse_quote_record(&json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("route.swaps[1].protocol"),
            "should reference index 1: {err}"
        );
    }
}
