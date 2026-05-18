//! Quote ingestion loader for the slippage feature dataset.
//!
//! Loads batches of raw quote records from historical data sources (JSONL
//! files), parsing and validating each record. Invalid records are captured
//! as errors alongside valid ones so callers can audit data quality without
//! losing the entire batch.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use crate::{
    quote_record::{self, ParseError, QuoteRecord},
    schema::{self, SchemaViolation},
};

/// Errors that prevent loading from starting or completing.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The data source could not be opened or read.
    #[error("source unavailable: {0}")]
    SourceUnavailable(#[from] std::io::Error),

    /// The source contained zero records (not even invalid ones).
    #[error("source is empty: no records found")]
    EmptySource,
}

/// Per-record outcome after parse + validation.
#[derive(Debug)]
pub enum RecordOutcome {
    /// Record parsed and passed schema validation.
    Valid(QuoteRecord),

    /// Record failed JSON parsing.
    ParseFailed { line: usize, raw: String, error: ParseError },

    /// Record parsed successfully but failed schema validation.
    InvalidRecord { line: usize, record: QuoteRecord, violations: Vec<SchemaViolation> },
}

impl RecordOutcome {
    /// Returns `true` if this outcome is a valid record.
    pub fn is_valid(&self) -> bool {
        matches!(self, RecordOutcome::Valid(_))
    }

    /// Returns the valid record if present.
    pub fn as_valid(&self) -> Option<&QuoteRecord> {
        match self {
            RecordOutcome::Valid(r) => Some(r),
            RecordOutcome::ParseFailed { .. } | RecordOutcome::InvalidRecord { .. } => None,
        }
    }

    /// Returns the 1-based line number that produced this outcome.
    pub fn line_number(&self) -> usize {
        match self {
            RecordOutcome::Valid(_) => 0,
            RecordOutcome::ParseFailed { line, .. } | RecordOutcome::InvalidRecord { line, .. } => {
                *line
            }
        }
    }
}

/// Result of loading a batch of records from a source.
#[derive(Debug)]
pub struct BatchResult {
    pub outcomes: Vec<RecordOutcome>,
}

impl BatchResult {
    /// Number of records that parsed and validated successfully.
    pub fn valid_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.is_valid())
            .count()
    }

    /// Number of records that failed parsing or validation.
    pub fn error_count(&self) -> usize {
        self.outcomes.len() - self.valid_count()
    }

    /// Total number of records processed (valid + invalid).
    pub fn total_count(&self) -> usize {
        self.outcomes.len()
    }

    /// Collect only the successfully validated records.
    pub fn into_valid_records(self) -> Vec<QuoteRecord> {
        self.outcomes
            .into_iter()
            .filter_map(|o| match o {
                RecordOutcome::Valid(r) => Some(r),
                RecordOutcome::ParseFailed { .. } | RecordOutcome::InvalidRecord { .. } => None,
            })
            .collect()
    }

    /// References to valid records without consuming the batch.
    pub fn valid_records(&self) -> Vec<&QuoteRecord> {
        self.outcomes
            .iter()
            .filter_map(RecordOutcome::as_valid)
            .collect()
    }
}

/// Load all quote records from a buffered reader of JSONL data.
///
/// Each line is treated as a separate JSON record. Blank lines are skipped.
/// Returns one [`RecordOutcome`] per non-blank line. The caller decides how
/// to handle mixed valid/invalid results.
pub fn load_records_from_reader<R: BufRead>(reader: R) -> Vec<RecordOutcome> {
    let mut outcomes = Vec::new();

    for (index, line_result) in reader.lines().enumerate() {
        let line_number = index + 1;

        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                outcomes.push(RecordOutcome::ParseFailed {
                    line: line_number,
                    raw: String::new(),
                    error: ParseError::InvalidJson(serde_json::Error::io(e)),
                });
                continue;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match quote_record::parse_quote_record(trimmed) {
            Err(error) => {
                outcomes.push(RecordOutcome::ParseFailed {
                    line: line_number,
                    raw: trimmed.to_owned(),
                    error,
                });
            }
            Ok(record) => match schema::validate_quote_record(&record) {
                Ok(()) => outcomes.push(RecordOutcome::Valid(record)),
                Err(violations) => {
                    outcomes.push(RecordOutcome::InvalidRecord {
                        line: line_number,
                        record,
                        violations,
                    });
                }
            },
        }
    }

    outcomes
}

/// Load all quote records from a JSONL file at the given path.
///
/// Returns [`LoadError::SourceUnavailable`] when the file cannot be opened,
/// and [`LoadError::EmptySource`] when the file contains no non-blank lines.
pub fn load_records_from_path(path: &Path) -> Result<BatchResult, LoadError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let outcomes = load_records_from_reader(reader);

    if outcomes.is_empty() {
        return Err(LoadError::EmptySource);
    }

    Ok(BatchResult { outcomes })
}

/// Load quote records from a JSONL string (convenience for testing).
///
/// Returns [`LoadError::EmptySource`] when the input has no non-blank lines.
pub fn load_records_from_string(data: &str) -> Result<BatchResult, LoadError> {
    let reader = std::io::Cursor::new(data);
    let outcomes = load_records_from_reader(BufReader::new(reader));

    if outcomes.is_empty() {
        return Err(LoadError::EmptySource);
    }

    Ok(BatchResult { outcomes })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn valid_eth_json() -> String {
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

    fn valid_base_json() -> String {
        serde_json::json!({
            "quote_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "chain_id": 8453,
            "block": {
                "number": 5000000,
                "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                "timestamp": 1700000000
            },
            "route": {
                "swaps": [{
                    "component_id": "0xaaaa1111bbbb2222cccc3333dddd4444eeee5555",
                    "protocol": "uniswap_v3",
                    "token_in": "0x4200000000000000000000000000000000000006",
                    "token_out": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
                    "amount_in": "500",
                    "amount_out": "400",
                    "gas_estimate": "80000",
                    "split": 1.0
                }]
            },
            "amount_in": "500",
            "amount_out": "400",
            "gas_estimate": "80000"
        })
        .to_string()
    }

    fn malformed_json_line() -> &'static str {
        "{not valid json at all"
    }

    fn missing_field_json() -> String {
        serde_json::json!({
            "chain_id": 1,
            "block": { "number": 100, "hash": "0xabc", "timestamp": 100 }
        })
        .to_string()
    }

    fn unsupported_chain_json() -> String {
        serde_json::json!({
            "quote_id": "deadbeef-0000-0000-0000-000000000000",
            "chain_id": 42161,
            "block": {
                "number": 100000,
                "hash": "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                "timestamp": 1700000000
            },
            "route": {
                "swaps": [{
                    "component_id": "0x1234567890abcdef1234567890abcdef12345678",
                    "protocol": "uniswap_v3",
                    "token_in": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "token_out": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "amount_in": "1000",
                    "amount_out": "900",
                    "gas_estimate": "100000",
                    "split": 1.0
                }]
            },
            "amount_in": "1000",
            "amount_out": "900",
            "gas_estimate": "100000"
        })
        .to_string()
    }

    fn write_temp_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file.flush().expect("flush");
        file
    }

    // ---- End-to-end loading of well-formed data ----

    #[test]
    fn load_single_valid_record_from_string() {
        let data = valid_eth_json();
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 1);
        assert_eq!(batch.valid_count(), 1);
        assert_eq!(batch.error_count(), 0);

        let records = batch.into_valid_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].chain_id, 1);
        assert_eq!(records[0].block_number(), 21_000_000);
    }

    #[test]
    fn load_multiple_valid_records_from_string() {
        let data = format!("{}\n{}", valid_eth_json(), valid_base_json());
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 2);
        assert_eq!(batch.valid_count(), 2);
        assert_eq!(batch.error_count(), 0);

        let records = batch.valid_records();
        assert_eq!(records[0].chain_id, 1);
        assert_eq!(records[1].chain_id, 8453);
    }

    #[test]
    fn load_valid_records_from_file() {
        let eth = valid_eth_json();
        let base = valid_base_json();
        let file = write_temp_jsonl(&[&eth, &base]);

        let batch = load_records_from_path(file.path()).expect("should load");
        assert_eq!(batch.total_count(), 2);
        assert_eq!(batch.valid_count(), 2);
        assert_eq!(batch.error_count(), 0);
    }

    #[test]
    fn load_preserves_record_fields() {
        let data = valid_eth_json();
        let batch = load_records_from_string(&data).expect("should load");
        let records = batch.into_valid_records();
        let record = &records[0];

        assert_eq!(record.quote_id, "f47ac10b-58cc-4372-a567-0e02b2c3d479");
        assert_eq!(record.chain_id, 1);
        assert_eq!(record.block.number, 21_000_000);
        assert_eq!(record.amount_in, "1000000000000000000");
        assert_eq!(record.amount_out, "3500000000");
        assert_eq!(record.gas_estimate, "150000");
        assert_eq!(record.route.hop_count(), 1);
        assert_eq!(record.route.swaps[0].protocol, "uniswap_v2");
    }

    #[test]
    fn load_skips_blank_lines() {
        let data = format!("\n{}\n\n{}\n\n", valid_eth_json(), valid_base_json());
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 2);
        assert_eq!(batch.valid_count(), 2);
    }

    // ---- Graceful handling of mixed valid/invalid records ----

    #[test]
    fn mixed_valid_and_malformed_json() {
        let data =
            format!("{}\n{}\n{}", valid_eth_json(), malformed_json_line(), valid_base_json());
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 3);
        assert_eq!(batch.valid_count(), 2);
        assert_eq!(batch.error_count(), 1);

        assert!(matches!(batch.outcomes[1], RecordOutcome::ParseFailed { line: 2, .. }));
    }

    #[test]
    fn mixed_valid_and_missing_field() {
        let data = format!("{}\n{}", valid_eth_json(), missing_field_json());
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 2);
        assert_eq!(batch.valid_count(), 1);
        assert_eq!(batch.error_count(), 1);

        assert!(matches!(batch.outcomes[1], RecordOutcome::ParseFailed { line: 2, .. }));
    }

    #[test]
    fn mixed_valid_and_schema_violation() {
        let data = format!("{}\n{}", valid_eth_json(), unsupported_chain_json());
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 2);
        assert_eq!(batch.valid_count(), 1);
        assert_eq!(batch.error_count(), 1);

        match &batch.outcomes[1] {
            RecordOutcome::InvalidRecord { line, violations, .. } => {
                assert_eq!(*line, 2);
                assert!(violations
                    .iter()
                    .any(|v| v.field == "chain_id"));
            }
            other => panic!("expected InvalidRecord, got {other:?}"),
        }
    }

    #[test]
    fn all_records_invalid_still_returns_batch() {
        let data = format!("{}\n{}", malformed_json_line(), unsupported_chain_json());
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 2);
        assert_eq!(batch.valid_count(), 0);
        assert_eq!(batch.error_count(), 2);
    }

    #[test]
    fn mixed_errors_preserve_line_numbers() {
        let data = format!(
            "{}\n{}\n{}\n{}\n{}",
            valid_eth_json(),
            malformed_json_line(),
            valid_base_json(),
            unsupported_chain_json(),
            valid_eth_json()
        );
        let batch = load_records_from_string(&data).expect("should load");
        assert_eq!(batch.total_count(), 5);
        assert_eq!(batch.valid_count(), 3);
        assert_eq!(batch.error_count(), 2);

        assert!(matches!(batch.outcomes[1], RecordOutcome::ParseFailed { line: 2, .. }));
        assert!(matches!(batch.outcomes[3], RecordOutcome::InvalidRecord { line: 4, .. }));
    }

    #[test]
    fn parse_failed_captures_raw_line() {
        let bad = malformed_json_line();
        let data = bad.to_string();
        let batch = load_records_from_string(&data).expect("should load");

        match &batch.outcomes[0] {
            RecordOutcome::ParseFailed { raw, .. } => {
                assert_eq!(raw, bad);
            }
            other => panic!("expected ParseFailed, got {other:?}"),
        }
    }

    #[test]
    fn invalid_record_retains_parsed_data() {
        let data = unsupported_chain_json();
        let batch = load_records_from_string(&data).expect("should load");

        match &batch.outcomes[0] {
            RecordOutcome::InvalidRecord { record, .. } => {
                assert_eq!(record.chain_id, 42161);
                assert_eq!(record.quote_id, "deadbeef-0000-0000-0000-000000000000");
            }
            other => panic!("expected InvalidRecord, got {other:?}"),
        }
    }

    // ---- Error behavior: source unavailable or empty ----

    #[test]
    fn error_on_nonexistent_file() {
        let result = load_records_from_path(Path::new("/nonexistent/path/quotes.jsonl"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, LoadError::SourceUnavailable(_)));
        assert!(err
            .to_string()
            .contains("source unavailable"));
    }

    #[test]
    fn error_on_empty_file() {
        let file = write_temp_jsonl(&[]);
        let result = load_records_from_path(file.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, LoadError::EmptySource));
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn error_on_blank_only_file() {
        let mut file = NamedTempFile::new().expect("create temp file");
        writeln!(file, "   ").expect("write");
        writeln!(file).expect("write");
        writeln!(file, "  ").expect("write");
        file.flush().expect("flush");

        let result = load_records_from_path(file.path());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoadError::EmptySource));
    }

    #[test]
    fn error_on_empty_string_input() {
        let result = load_records_from_string("");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoadError::EmptySource));
    }

    #[test]
    fn error_on_whitespace_only_string() {
        let result = load_records_from_string("  \n\n  \n");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoadError::EmptySource));
    }

    // ---- BatchResult helpers ----

    #[test]
    fn batch_result_valid_records_returns_references() {
        let data = format!("{}\n{}", valid_eth_json(), valid_base_json());
        let batch = load_records_from_string(&data).expect("should load");
        let refs = batch.valid_records();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].chain_id, 1);
        assert_eq!(refs[1].chain_id, 8453);
    }

    #[test]
    fn batch_result_into_valid_records_filters_errors() {
        let data =
            format!("{}\n{}\n{}", valid_eth_json(), malformed_json_line(), valid_base_json());
        let batch = load_records_from_string(&data).expect("should load");
        let records = batch.into_valid_records();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn record_outcome_is_valid_predicate() {
        let data = format!("{}\n{}", valid_eth_json(), malformed_json_line());
        let batch = load_records_from_string(&data).expect("should load");
        assert!(batch.outcomes[0].is_valid());
        assert!(!batch.outcomes[1].is_valid());
    }

    // ---- File-based end-to-end ----

    #[test]
    fn file_based_mixed_batch_end_to_end() {
        let eth = valid_eth_json();
        let base = valid_base_json();
        let bad_chain = unsupported_chain_json();
        let file = write_temp_jsonl(&[&eth, malformed_json_line(), &base, &bad_chain]);

        let batch = load_records_from_path(file.path()).expect("should load");
        assert_eq!(batch.total_count(), 4);
        assert_eq!(batch.valid_count(), 2);
        assert_eq!(batch.error_count(), 2);

        let records = batch.into_valid_records();
        assert_eq!(records[0].chain_id, 1);
        assert_eq!(records[1].chain_id, 8453);
    }
}
