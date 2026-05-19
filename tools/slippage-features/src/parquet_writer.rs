use std::{path::Path, sync::Arc};

use arrow::{
    array::{ArrayRef, BooleanBuilder, Float64Builder, StringBuilder, UInt32Builder, UInt64Builder},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use fynd_core::observer::QuoteProducedEvent;
use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};

use crate::quote_log::route_to_json;

/// Per-hop decay record for a single (quote, block_offset, hop) triple.
#[derive(Debug, Clone)]
pub struct HopDecayRecord {
    /// Links to quote log.
    pub quote_id: String,
    /// Which solver produced this quote.
    pub solver_id: String,
    /// Groups solver responses for the same request.
    pub request_id: String,
    /// Block offset k in 1..=MAX_BLOCK_OFFSET.
    pub block_offset: u32,
    /// 0-based index in route.
    pub hop_index: u32,
    /// Pool address (component ID).
    pub component_id: String,
    /// Pool type (e.g. "uniswap_v3", "curve").
    pub protocol: String,
    /// Simulated output at block X+k (BigInt string).
    pub hop_amount_out: String,
    /// Per-hop decay in basis points.
    pub hop_decay_bps: f64,
    /// Pool depth at 1% (BigInt string, nullable).
    pub depth_at_1pct: Option<String>,
    /// Pool depth at 5% (BigInt string, nullable).
    pub depth_at_5pct: Option<String>,
    /// Pool spot price (nullable).
    pub spot_price: Option<f64>,
    /// Token price in native currency (nullable).
    pub token_price_in_native: Option<f64>,
    /// Pool fee (nullable).
    pub fee_tier: Option<f64>,
    /// Marginal liquidity for v3/v4 pools (BigInt string, nullable).
    pub marginal_liquidity: Option<String>,
    /// Liquidity distribution metric for v3/v4 (nullable).
    pub concentration_gini: Option<f64>,
    /// Full route output at block X+k (BigInt string).
    pub route_total_amount_out: String,
    /// Full route decay in basis points.
    pub route_decay_bps: f64,
}

/// Returns the Arrow schema for hop decay records.
pub fn hop_decay_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("request_id", DataType::Utf8, false),
        Field::new("block_offset", DataType::UInt32, false),
        Field::new("hop_index", DataType::UInt32, false),
        Field::new("component_id", DataType::Utf8, false),
        Field::new("protocol", DataType::Utf8, false),
        Field::new("hop_amount_out", DataType::Utf8, false),
        Field::new("hop_decay_bps", DataType::Float64, false),
        Field::new("depth_at_1pct", DataType::Utf8, true),
        Field::new("depth_at_5pct", DataType::Utf8, true),
        Field::new("spot_price", DataType::Float64, true),
        Field::new("token_price_in_native", DataType::Float64, true),
        Field::new("fee_tier", DataType::Float64, true),
        Field::new("marginal_liquidity", DataType::Utf8, true),
        Field::new("concentration_gini", DataType::Float64, true),
        Field::new("route_total_amount_out", DataType::Utf8, false),
        Field::new("route_decay_bps", DataType::Float64, false),
    ])
}

/// Write hop decay records to a parquet file.
///
/// # Errors
///
/// Returns an error if the file cannot be created, the record batch
/// construction fails, or writing to the file fails.
pub fn write_hop_decay_parquet(
    path: &Path,
    records: &[HopDecayRecord],
) -> Result<(), ParquetWriteError> {
    let schema = Arc::new(hop_decay_schema());
    let n = records.len();

    let mut quote_id = StringBuilder::with_capacity(n, n * 36);
    let mut solver_id = StringBuilder::with_capacity(n, n * 20);
    let mut request_id = StringBuilder::with_capacity(n, n * 36);
    let mut block_offset = UInt32Builder::with_capacity(n);
    let mut hop_index = UInt32Builder::with_capacity(n);
    let mut component_id = StringBuilder::with_capacity(n, n * 42);
    let mut protocol = StringBuilder::with_capacity(n, n * 16);
    let mut hop_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut hop_decay_bps = Float64Builder::with_capacity(n);
    let mut depth_at_1pct = StringBuilder::with_capacity(n, n * 32);
    let mut depth_at_5pct = StringBuilder::with_capacity(n, n * 32);
    let mut spot_price = Float64Builder::with_capacity(n);
    let mut token_price_in_native = Float64Builder::with_capacity(n);
    let mut fee_tier = Float64Builder::with_capacity(n);
    let mut marginal_liquidity = StringBuilder::with_capacity(n, n * 32);
    let mut concentration_gini = Float64Builder::with_capacity(n);
    let mut route_total_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut route_decay_bps = Float64Builder::with_capacity(n);

    for r in records {
        quote_id.append_value(&r.quote_id);
        solver_id.append_value(&r.solver_id);
        request_id.append_value(&r.request_id);
        block_offset.append_value(r.block_offset);
        hop_index.append_value(r.hop_index);
        component_id.append_value(&r.component_id);
        protocol.append_value(&r.protocol);
        hop_amount_out.append_value(&r.hop_amount_out);
        hop_decay_bps.append_value(r.hop_decay_bps);

        match &r.depth_at_1pct {
            Some(v) => depth_at_1pct.append_value(v),
            None => depth_at_1pct.append_null(),
        }
        match &r.depth_at_5pct {
            Some(v) => depth_at_5pct.append_value(v),
            None => depth_at_5pct.append_null(),
        }
        match r.spot_price {
            Some(v) => spot_price.append_value(v),
            None => spot_price.append_null(),
        }
        match r.token_price_in_native {
            Some(v) => token_price_in_native.append_value(v),
            None => token_price_in_native.append_null(),
        }
        match r.fee_tier {
            Some(v) => fee_tier.append_value(v),
            None => fee_tier.append_null(),
        }
        match &r.marginal_liquidity {
            Some(v) => marginal_liquidity.append_value(v),
            None => marginal_liquidity.append_null(),
        }
        match r.concentration_gini {
            Some(v) => concentration_gini.append_value(v),
            None => concentration_gini.append_null(),
        }
        route_total_amount_out.append_value(&r.route_total_amount_out);
        route_decay_bps.append_value(r.route_decay_bps);
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(quote_id.finish()),
        Arc::new(solver_id.finish()),
        Arc::new(request_id.finish()),
        Arc::new(block_offset.finish()),
        Arc::new(hop_index.finish()),
        Arc::new(component_id.finish()),
        Arc::new(protocol.finish()),
        Arc::new(hop_amount_out.finish()),
        Arc::new(hop_decay_bps.finish()),
        Arc::new(depth_at_1pct.finish()),
        Arc::new(depth_at_5pct.finish()),
        Arc::new(spot_price.finish()),
        Arc::new(token_price_in_native.finish()),
        Arc::new(fee_tier.finish()),
        Arc::new(marginal_liquidity.finish()),
        Arc::new(concentration_gini.finish()),
        Arc::new(route_total_amount_out.finish()),
        Arc::new(route_decay_bps.finish()),
    ];

    let batch = RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| ParquetWriteError::BatchConstruction(e.to_string()))?;

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();

    let file = std::fs::File::create(path).map_err(|e| ParquetWriteError::Io(e.to_string()))?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))
        .map_err(|e| ParquetWriteError::Writer(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| ParquetWriteError::Writer(e.to_string()))?;
    writer
        .close()
        .map_err(|e| ParquetWriteError::Writer(e.to_string()))?;

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ParquetWriteError {
    #[error("failed to create record batch: {0}")]
    BatchConstruction(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("parquet writer error: {0}")]
    Writer(String),
}

// ---------------------------------------------------------------------------
// Quote log parquet
// ---------------------------------------------------------------------------

/// A single row in the quote log parquet file.
#[derive(Debug, Clone)]
pub struct QuoteLogRecord {
    pub quote_id: String,
    pub solver_id: String,
    pub request_id: String,
    pub is_winner: bool,
    pub block_number: u64,
    pub chain_id: u64,
    pub amount_in: String,
    pub amount_out: String,
    pub gas_estimate: u64,
    pub algorithm_type: String,
    pub n_alternatives: u32,
    pub gap_to_second_best_bps: Option<f64>,
    pub slippage_tolerance: Option<f64>,
    pub route_json: String,
    pub calldata_hex: String,
}

impl From<&QuoteProducedEvent> for QuoteLogRecord {
    fn from(event: &QuoteProducedEvent) -> Self {
        Self {
            quote_id: event.quote_id.clone(),
            solver_id: event.solver_id.clone(),
            request_id: event.request_id.clone(),
            is_winner: event.is_winner,
            block_number: event.block_number,
            chain_id: event.chain_id,
            amount_in: event.amount_in.clone(),
            amount_out: event.amount_out.clone(),
            gas_estimate: event.gas_estimate,
            algorithm_type: event.algorithm_type.clone(),
            n_alternatives: event.n_alternatives,
            gap_to_second_best_bps: event.gap_to_second_best_bps,
            slippage_tolerance: event.slippage_tolerance,
            route_json: route_to_json(&event.route),
            calldata_hex: hex::encode(&event.calldata),
        }
    }
}

/// Returns the Arrow schema for quote log records.
pub fn quote_log_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("request_id", DataType::Utf8, false),
        Field::new("is_winner", DataType::Boolean, false),
        Field::new("block_number", DataType::UInt64, false),
        Field::new("chain_id", DataType::UInt64, false),
        Field::new("amount_in", DataType::Utf8, false),
        Field::new("amount_out", DataType::Utf8, false),
        Field::new("gas_estimate", DataType::UInt64, false),
        Field::new("algorithm_type", DataType::Utf8, false),
        Field::new("n_alternatives", DataType::UInt32, false),
        Field::new("gap_to_second_best_bps", DataType::Float64, true),
        Field::new("slippage_tolerance", DataType::Float64, true),
        Field::new("route_json", DataType::Utf8, false),
        Field::new("calldata_hex", DataType::Utf8, false),
    ])
}

/// Write quote log records to a parquet file.
///
/// # Errors
///
/// Returns an error if the file cannot be created, the record batch
/// construction fails, or writing to the file fails.
pub fn write_quote_log_parquet(
    path: &Path,
    records: &[QuoteLogRecord],
) -> Result<(), ParquetWriteError> {
    let schema = Arc::new(quote_log_schema());
    let n = records.len();

    let mut quote_id = StringBuilder::with_capacity(n, n * 36);
    let mut solver_id = StringBuilder::with_capacity(n, n * 20);
    let mut request_id = StringBuilder::with_capacity(n, n * 36);
    let mut is_winner = BooleanBuilder::with_capacity(n);
    let mut block_number = UInt64Builder::with_capacity(n);
    let mut chain_id = UInt64Builder::with_capacity(n);
    let mut amount_in = StringBuilder::with_capacity(n, n * 32);
    let mut amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut gas_estimate = UInt64Builder::with_capacity(n);
    let mut algorithm_type = StringBuilder::with_capacity(n, n * 20);
    let mut n_alternatives = UInt32Builder::with_capacity(n);
    let mut gap_to_second_best_bps = Float64Builder::with_capacity(n);
    let mut slippage_tolerance = Float64Builder::with_capacity(n);
    let mut route_json = StringBuilder::with_capacity(n, n * 256);
    let mut calldata_hex = StringBuilder::with_capacity(n, n * 128);

    for r in records {
        quote_id.append_value(&r.quote_id);
        solver_id.append_value(&r.solver_id);
        request_id.append_value(&r.request_id);
        is_winner.append_value(r.is_winner);
        block_number.append_value(r.block_number);
        chain_id.append_value(r.chain_id);
        amount_in.append_value(&r.amount_in);
        amount_out.append_value(&r.amount_out);
        gas_estimate.append_value(r.gas_estimate);
        algorithm_type.append_value(&r.algorithm_type);
        n_alternatives.append_value(r.n_alternatives);

        match r.gap_to_second_best_bps {
            Some(v) => gap_to_second_best_bps.append_value(v),
            None => gap_to_second_best_bps.append_null(),
        }
        match r.slippage_tolerance {
            Some(v) => slippage_tolerance.append_value(v),
            None => slippage_tolerance.append_null(),
        }

        route_json.append_value(&r.route_json);
        calldata_hex.append_value(&r.calldata_hex);
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(quote_id.finish()),
        Arc::new(solver_id.finish()),
        Arc::new(request_id.finish()),
        Arc::new(is_winner.finish()),
        Arc::new(block_number.finish()),
        Arc::new(chain_id.finish()),
        Arc::new(amount_in.finish()),
        Arc::new(amount_out.finish()),
        Arc::new(gas_estimate.finish()),
        Arc::new(algorithm_type.finish()),
        Arc::new(n_alternatives.finish()),
        Arc::new(gap_to_second_best_bps.finish()),
        Arc::new(slippage_tolerance.finish()),
        Arc::new(route_json.finish()),
        Arc::new(calldata_hex.finish()),
    ];

    let batch = RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| ParquetWriteError::BatchConstruction(e.to_string()))?;

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();

    let file = std::fs::File::create(path).map_err(|e| ParquetWriteError::Io(e.to_string()))?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))
        .map_err(|e| ParquetWriteError::Writer(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| ParquetWriteError::Writer(e.to_string()))?;
    writer
        .close()
        .map_err(|e| ParquetWriteError::Writer(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use arrow::array::{Array, Float64Array, StringArray, UInt32Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;

    fn sample_record(quote_id: &str, block_offset: u32, hop_index: u32) -> HopDecayRecord {
        HopDecayRecord {
            quote_id: quote_id.to_string(),
            solver_id: "solver-a".to_string(),
            request_id: "req-1".to_string(),
            block_offset,
            hop_index,
            component_id: "0xpool1".to_string(),
            protocol: "uniswap_v2".to_string(),
            hop_amount_out: "990".to_string(),
            hop_decay_bps: 10.0,
            depth_at_1pct: Some("50000".to_string()),
            depth_at_5pct: None,
            spot_price: Some(1.5),
            token_price_in_native: None,
            fee_tier: Some(0.003),
            marginal_liquidity: None,
            concentration_gini: None,
            route_total_amount_out: "980".to_string(),
            route_decay_bps: 20.0,
        }
    }

    #[test]
    fn schema_has_expected_field_count() {
        let schema = hop_decay_schema();
        assert_eq!(schema.fields().len(), 18);
    }

    #[test]
    fn round_trip_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.parquet");

        let records =
            vec![sample_record("q1", 1, 0), sample_record("q1", 1, 1), sample_record("q2", 2, 0)];

        write_hop_decay_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 18);

        let quote_ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(quote_ids.value(0), "q1");
        assert_eq!(quote_ids.value(2), "q2");

        let offsets = batch
            .column(3)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(offsets.value(0), 1);
        assert_eq!(offsets.value(2), 2);

        let hop_indices = batch
            .column(4)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(hop_indices.value(0), 0);
        assert_eq!(hop_indices.value(1), 1);

        let decay = batch
            .column(8)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((decay.value(0) - 10.0).abs() < f64::EPSILON);

        // Nullable columns: depth_at_1pct has value for first, depth_at_5pct is null
        let depth_1 = batch
            .column(9)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(depth_1.value(0), "50000");

        let depth_5 = batch
            .column(10)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(depth_5.is_null(0));
    }

    #[test]
    fn empty_records_produce_valid_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.parquet");

        write_hop_decay_parquet(&path, &[]).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();

        // Schema should have 18 fields.
        assert_eq!(builder.schema().fields().len(), 18);

        let reader = builder.build().unwrap();
        let total_rows: usize = reader
            .filter_map(|b| b.ok())
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total_rows, 0);
    }

    // -----------------------------------------------------------------------
    // Quote log parquet tests
    // -----------------------------------------------------------------------

    fn sample_quote_log_record(quote_id: &str) -> QuoteLogRecord {
        QuoteLogRecord {
            quote_id: quote_id.to_string(),
            solver_id: "solver-a".to_string(),
            request_id: "req-1".to_string(),
            is_winner: true,
            block_number: 100,
            chain_id: 1,
            amount_in: "1000".to_string(),
            amount_out: "990".to_string(),
            gas_estimate: 100_000,
            algorithm_type: "most_liquid".to_string(),
            n_alternatives: 3,
            gap_to_second_best_bps: Some(10.0),
            slippage_tolerance: None,
            route_json: r#"{"swaps":[]}"#.to_string(),
            calldata_hex: "abcd".to_string(),
        }
    }

    #[test]
    fn quote_log_schema_has_expected_field_count() {
        let schema = quote_log_schema();
        assert_eq!(schema.fields().len(), 15);
    }

    #[test]
    fn quote_log_round_trip() {
        use arrow::array::{BooleanArray, UInt64Array};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quote_log.parquet");

        let records = vec![
            sample_quote_log_record("q1"),
            {
                let mut r = sample_quote_log_record("q2");
                r.is_winner = false;
                r.gap_to_second_best_bps = None;
                r.slippage_tolerance = Some(0.005);
                r
            },
        ];

        write_quote_log_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 15);

        let quote_ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(quote_ids.value(0), "q1");
        assert_eq!(quote_ids.value(1), "q2");

        let winners = batch
            .column(3)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(winners.value(0));
        assert!(!winners.value(1));

        let blocks = batch
            .column(4)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(blocks.value(0), 100);

        // gap_to_second_best_bps: first has value, second is null
        let gap = batch
            .column(11)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!(!gap.is_null(0));
        assert!((gap.value(0) - 10.0).abs() < f64::EPSILON);
        assert!(gap.is_null(1));

        // slippage_tolerance: first is null, second has value
        let slip = batch
            .column(12)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!(slip.is_null(0));
        assert!(!slip.is_null(1));
        assert!((slip.value(1) - 0.005).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_quote_log_produces_valid_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty_quote.parquet");

        write_quote_log_parquet(&path, &[]).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert_eq!(builder.schema().fields().len(), 15);

        let reader = builder.build().unwrap();
        let total_rows: usize = reader
            .filter_map(|b| b.ok())
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total_rows, 0);
    }
}
