use std::{path::Path, sync::Arc};

use arrow::{
    array::{
        ArrayRef, BooleanBuilder, Float64Builder, StringBuilder, UInt32Builder, UInt64Builder,
    },
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use fynd_core::observer::QuoteProducedEvent;
use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};

use crate::quote_log::route_to_json;

/// Per-hop volatile data for a single (quote, block_offset, hop) triple.
///
/// Only contains fields that change at each block offset. Static hop
/// metadata lives in `HopStaticRecord`; route-level aggregates live in
/// `TychoRouteDecayRecord`.
#[derive(Debug, Clone)]
pub struct HopDecayRecord {
    pub quote_id: String,
    pub solver_id: String,
    pub block_offset: u32,
    pub hop_index: u32,
    pub hop_amount_out: String,
    pub hop_decay_bps: f64,
    pub depth_at_1pct: Option<String>,
    pub depth_at_5pct: Option<String>,
    pub spot_price: Option<f64>,
    pub token_price_in_native: Option<f64>,
}

/// Static metadata for a single hop in a quote's route. Written once per
/// hop (not repeated across block offsets).
#[derive(Debug, Clone)]
pub struct HopStaticRecord {
    pub quote_id: String,
    pub solver_id: String,
    pub hop_index: u32,
    pub component_id: String,
    pub protocol: String,
    pub fee_tier: Option<f64>,
}

/// Route-level decay from Tycho resim at a single block offset.
///
/// Separated from `HopDecayRecord` because these values are the same
/// across all hops at a given block offset.
#[derive(Debug, Clone)]
pub struct TychoRouteDecayRecord {
    pub quote_id: String,
    pub solver_id: String,
    pub block_offset: u32,
    pub route_total_amount_out: String,
    pub route_decay_bps: f64,
    pub requote_amount_out: Option<String>,
    pub market_movement_bps: Option<f64>,
    pub execution_slippage_bps: Option<f64>,
}

pub fn hop_decay_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("block_offset", DataType::UInt32, false),
        Field::new("hop_index", DataType::UInt32, false),
        Field::new("hop_amount_out", DataType::Utf8, false),
        Field::new("hop_decay_bps", DataType::Float64, false),
        Field::new("depth_at_1pct", DataType::Utf8, true),
        Field::new("depth_at_5pct", DataType::Utf8, true),
        Field::new("spot_price", DataType::Float64, true),
        Field::new("token_price_in_native", DataType::Float64, true),
    ])
}

pub fn hop_static_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("hop_index", DataType::UInt32, false),
        Field::new("component_id", DataType::Utf8, false),
        Field::new("protocol", DataType::Utf8, false),
        Field::new("fee_tier", DataType::Float64, true),
    ])
}

pub fn tycho_route_decay_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("block_offset", DataType::UInt32, false),
        Field::new("route_total_amount_out", DataType::Utf8, false),
        Field::new("route_decay_bps", DataType::Float64, false),
        Field::new("requote_amount_out", DataType::Utf8, true),
        Field::new("market_movement_bps", DataType::Float64, true),
        Field::new("execution_slippage_bps", DataType::Float64, true),
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
    let mut block_offset = UInt32Builder::with_capacity(n);
    let mut hop_index = UInt32Builder::with_capacity(n);
    let mut hop_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut hop_decay_bps = Float64Builder::with_capacity(n);
    let mut depth_at_1pct = StringBuilder::with_capacity(n, n * 32);
    let mut depth_at_5pct = StringBuilder::with_capacity(n, n * 32);
    let mut spot_price = Float64Builder::with_capacity(n);
    let mut token_price_in_native = Float64Builder::with_capacity(n);

    for r in records {
        quote_id.append_value(&r.quote_id);
        solver_id.append_value(&r.solver_id);
        block_offset.append_value(r.block_offset);
        hop_index.append_value(r.hop_index);
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
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(quote_id.finish()),
        Arc::new(solver_id.finish()),
        Arc::new(block_offset.finish()),
        Arc::new(hop_index.finish()),
        Arc::new(hop_amount_out.finish()),
        Arc::new(hop_decay_bps.finish()),
        Arc::new(depth_at_1pct.finish()),
        Arc::new(depth_at_5pct.finish()),
        Arc::new(spot_price.finish()),
        Arc::new(token_price_in_native.finish()),
    ];

    write_parquet(path, schema, columns)
}

/// Write hop static records to a parquet file.
///
/// # Errors
///
/// Returns an error if the file cannot be created, the record batch
/// construction fails, or writing to the file fails.
pub fn write_hop_static_parquet(
    path: &Path,
    records: &[HopStaticRecord],
) -> Result<(), ParquetWriteError> {
    let schema = Arc::new(hop_static_schema());
    let n = records.len();

    let mut quote_id = StringBuilder::with_capacity(n, n * 36);
    let mut solver_id = StringBuilder::with_capacity(n, n * 20);
    let mut hop_index = UInt32Builder::with_capacity(n);
    let mut component_id = StringBuilder::with_capacity(n, n * 42);
    let mut protocol = StringBuilder::with_capacity(n, n * 16);
    let mut fee_tier = Float64Builder::with_capacity(n);

    for r in records {
        quote_id.append_value(&r.quote_id);
        solver_id.append_value(&r.solver_id);
        hop_index.append_value(r.hop_index);
        component_id.append_value(&r.component_id);
        protocol.append_value(&r.protocol);

        match r.fee_tier {
            Some(v) => fee_tier.append_value(v),
            None => fee_tier.append_null(),
        }
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(quote_id.finish()),
        Arc::new(solver_id.finish()),
        Arc::new(hop_index.finish()),
        Arc::new(component_id.finish()),
        Arc::new(protocol.finish()),
        Arc::new(fee_tier.finish()),
    ];

    write_parquet(path, schema, columns)
}

/// Write Tycho route-level decay records to a parquet file.
///
/// # Errors
///
/// Returns an error if the file cannot be created, the record batch
/// construction fails, or writing to the file fails.
pub fn write_tycho_route_decay_parquet(
    path: &Path,
    records: &[TychoRouteDecayRecord],
) -> Result<(), ParquetWriteError> {
    let schema = Arc::new(tycho_route_decay_schema());
    let n = records.len();

    let mut quote_id = StringBuilder::with_capacity(n, n * 36);
    let mut solver_id = StringBuilder::with_capacity(n, n * 20);
    let mut block_offset = UInt32Builder::with_capacity(n);
    let mut route_total_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut route_decay_bps = Float64Builder::with_capacity(n);
    let mut requote_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut market_movement_bps = Float64Builder::with_capacity(n);
    let mut execution_slippage_bps = Float64Builder::with_capacity(n);

    for r in records {
        quote_id.append_value(&r.quote_id);
        solver_id.append_value(&r.solver_id);
        block_offset.append_value(r.block_offset);
        route_total_amount_out.append_value(&r.route_total_amount_out);
        route_decay_bps.append_value(r.route_decay_bps);

        match &r.requote_amount_out {
            Some(v) => requote_amount_out.append_value(v),
            None => requote_amount_out.append_null(),
        }
        match r.market_movement_bps {
            Some(v) => market_movement_bps.append_value(v),
            None => market_movement_bps.append_null(),
        }
        match r.execution_slippage_bps {
            Some(v) => execution_slippage_bps.append_value(v),
            None => execution_slippage_bps.append_null(),
        }
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(quote_id.finish()),
        Arc::new(solver_id.finish()),
        Arc::new(block_offset.finish()),
        Arc::new(route_total_amount_out.finish()),
        Arc::new(route_decay_bps.finish()),
        Arc::new(requote_amount_out.finish()),
        Arc::new(market_movement_bps.finish()),
        Arc::new(execution_slippage_bps.finish()),
    ];

    write_parquet(path, schema, columns)
}

fn write_parquet(
    path: &Path,
    schema: Arc<Schema>,
    columns: Vec<ArrayRef>,
) -> Result<(), ParquetWriteError> {
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

// ---------------------------------------------------------------------------
// Route-level decay parquet (node resim output)
// ---------------------------------------------------------------------------

/// A single row in the route-level decay parquet produced by node resim.
#[derive(Debug, Clone)]
pub struct RouteDecayRecord {
    /// Links to quote log.
    pub quote_id: String,
    /// Which solver produced this quote.
    pub solver_id: String,
    /// Groups solver responses for the same request.
    pub request_id: String,
    /// Block offset k in 1..=MAX_BLOCK_OFFSET.
    pub block_offset: u32,
    /// Output amount from eth_call (BigInt string).
    pub eth_call_amount_out: String,
    /// Gas consumed by the eth_call.
    pub eth_call_gas_used: u64,
    /// Whether the eth_call succeeded (no structural revert).
    pub eth_call_success: bool,
    /// Revert reason if the call reverted (nullable).
    pub eth_call_revert_reason: Option<String>,
    /// Route decay in basis points.
    pub eth_call_decay_bps: f64,
}

/// Returns the Arrow schema for route-level decay records.
pub fn route_decay_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("request_id", DataType::Utf8, false),
        Field::new("block_offset", DataType::UInt32, false),
        Field::new("eth_call_amount_out", DataType::Utf8, false),
        Field::new("eth_call_gas_used", DataType::UInt64, false),
        Field::new("eth_call_success", DataType::Boolean, false),
        Field::new("eth_call_revert_reason", DataType::Utf8, true),
        Field::new("eth_call_decay_bps", DataType::Float64, false),
    ])
}

/// Write route-level decay records to a parquet file.
///
/// # Errors
///
/// Returns an error if the file cannot be created, the record batch
/// construction fails, or writing to the file fails.
pub fn write_route_decay_parquet(
    path: &Path,
    records: &[RouteDecayRecord],
) -> Result<(), ParquetWriteError> {
    let schema = Arc::new(route_decay_schema());
    let n = records.len();

    let mut quote_id = StringBuilder::with_capacity(n, n * 36);
    let mut solver_id = StringBuilder::with_capacity(n, n * 20);
    let mut request_id = StringBuilder::with_capacity(n, n * 36);
    let mut block_offset = UInt32Builder::with_capacity(n);
    let mut eth_call_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut eth_call_gas_used = UInt64Builder::with_capacity(n);
    let mut eth_call_success = BooleanBuilder::with_capacity(n);
    let mut eth_call_revert_reason = StringBuilder::with_capacity(n, n * 64);
    let mut eth_call_decay_bps = Float64Builder::with_capacity(n);

    for r in records {
        quote_id.append_value(&r.quote_id);
        solver_id.append_value(&r.solver_id);
        request_id.append_value(&r.request_id);
        block_offset.append_value(r.block_offset);
        eth_call_amount_out.append_value(&r.eth_call_amount_out);
        eth_call_gas_used.append_value(r.eth_call_gas_used);
        eth_call_success.append_value(r.eth_call_success);

        match &r.eth_call_revert_reason {
            Some(v) => eth_call_revert_reason.append_value(v),
            None => eth_call_revert_reason.append_null(),
        }

        eth_call_decay_bps.append_value(r.eth_call_decay_bps);
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(quote_id.finish()),
        Arc::new(solver_id.finish()),
        Arc::new(request_id.finish()),
        Arc::new(block_offset.finish()),
        Arc::new(eth_call_amount_out.finish()),
        Arc::new(eth_call_gas_used.finish()),
        Arc::new(eth_call_success.finish()),
        Arc::new(eth_call_revert_reason.finish()),
        Arc::new(eth_call_decay_bps.finish()),
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
    /// First hop token_in address (0x-prefixed hex).
    pub token_in: String,
    /// Last hop token_out address (0x-prefixed hex).
    pub token_out: String,
    pub route_json: String,
    pub calldata_hex: String,
}

impl From<&QuoteProducedEvent> for QuoteLogRecord {
    fn from(event: &QuoteProducedEvent) -> Self {
        let token_in = event
            .route
            .swaps
            .first()
            .map(|s| format!("0x{}", hex::encode(&s.token_in)))
            .unwrap_or_default();
        let token_out = event
            .route
            .swaps
            .last()
            .map(|s| format!("0x{}", hex::encode(&s.token_out)))
            .unwrap_or_default();

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
            token_in,
            token_out,
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
        Field::new("token_in", DataType::Utf8, false),
        Field::new("token_out", DataType::Utf8, false),
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
    let mut token_in = StringBuilder::with_capacity(n, n * 42);
    let mut token_out = StringBuilder::with_capacity(n, n * 42);
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

        token_in.append_value(&r.token_in);
        token_out.append_value(&r.token_out);
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
        Arc::new(token_in.finish()),
        Arc::new(token_out.finish()),
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

    fn sample_hop_decay(quote_id: &str, block_offset: u32, hop_index: u32) -> HopDecayRecord {
        HopDecayRecord {
            quote_id: quote_id.to_string(),
            solver_id: "solver-a".to_string(),
            block_offset,
            hop_index,
            hop_amount_out: "990".to_string(),
            hop_decay_bps: 10.0,
            depth_at_1pct: Some("50000".to_string()),
            depth_at_5pct: None,
            spot_price: Some(1.5),
            token_price_in_native: None,
        }
    }

    fn sample_hop_static(quote_id: &str, hop_index: u32) -> HopStaticRecord {
        HopStaticRecord {
            quote_id: quote_id.to_string(),
            solver_id: "solver-a".to_string(),
            hop_index,
            component_id: "0xpool1".to_string(),
            protocol: "uniswap_v2".to_string(),
            fee_tier: Some(0.003),
        }
    }

    fn sample_tycho_route_decay(quote_id: &str, block_offset: u32) -> TychoRouteDecayRecord {
        TychoRouteDecayRecord {
            quote_id: quote_id.to_string(),
            solver_id: "solver-a".to_string(),
            block_offset,
            route_total_amount_out: "980".to_string(),
            route_decay_bps: 20.0,
            requote_amount_out: None,
            market_movement_bps: None,
            execution_slippage_bps: None,
        }
    }

    #[test]
    fn hop_decay_schema_has_expected_field_count() {
        assert_eq!(hop_decay_schema().fields().len(), 10);
    }

    #[test]
    fn hop_static_schema_has_expected_field_count() {
        assert_eq!(hop_static_schema().fields().len(), 6);
    }

    #[test]
    fn tycho_route_decay_schema_has_expected_field_count() {
        assert_eq!(tycho_route_decay_schema().fields().len(), 8);
    }

    #[test]
    fn hop_decay_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.parquet");

        let records = vec![
            sample_hop_decay("q1", 1, 0),
            sample_hop_decay("q1", 1, 1),
            sample_hop_decay("q2", 2, 0),
        ];

        write_hop_decay_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 10);

        let quote_ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(quote_ids.value(0), "q1");
        assert_eq!(quote_ids.value(2), "q2");

        let offsets = batch
            .column(2)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(offsets.value(0), 1);
        assert_eq!(offsets.value(2), 2);

        let hop_indices = batch
            .column(3)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(hop_indices.value(0), 0);
        assert_eq!(hop_indices.value(1), 1);

        let decay = batch
            .column(5)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((decay.value(0) - 10.0).abs() < f64::EPSILON);

        let depth_1 = batch
            .column(6)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(depth_1.value(0), "50000");

        let depth_5 = batch
            .column(7)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(depth_5.is_null(0));
    }

    #[test]
    fn hop_static_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("static.parquet");

        let records = vec![sample_hop_static("q1", 0), sample_hop_static("q1", 1)];
        write_hop_static_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 6);

        let components = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(components.value(0), "0xpool1");

        let fees = batch
            .column(5)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((fees.value(0) - 0.003).abs() < f64::EPSILON);
    }

    #[test]
    fn tycho_route_decay_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tycho_route.parquet");

        let records = vec![
            sample_tycho_route_decay("q1", 1),
            sample_tycho_route_decay("q1", 2),
        ];
        write_tycho_route_decay_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 8);

        let decay = batch
            .column(4)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((decay.value(0) - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_records_produce_valid_parquet() {
        let dir = tempfile::tempdir().unwrap();

        write_hop_decay_parquet(&dir.path().join("empty_hop.parquet"), &[]).unwrap();
        write_hop_static_parquet(&dir.path().join("empty_static.parquet"), &[]).unwrap();
        write_tycho_route_decay_parquet(&dir.path().join("empty_tycho.parquet"), &[]).unwrap();

        for (name, expected_cols) in [
            ("empty_hop.parquet", 10),
            ("empty_static.parquet", 6),
            ("empty_tycho.parquet", 8),
        ] {
            let file = std::fs::File::open(dir.path().join(name)).unwrap();
            let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
            assert_eq!(builder.schema().fields().len(), expected_cols, "{name}");

            let reader = builder.build().unwrap();
            let total_rows: usize = reader
                .filter_map(|b| b.ok())
                .map(|b| b.num_rows())
                .sum();
            assert_eq!(total_rows, 0, "{name}");
        }
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
            token_in: "0x0101010101010101010101010101010101010101".to_string(),
            token_out: "0x0202020202020202020202020202020202020202".to_string(),
            route_json: r#"{"swaps":[]}"#.to_string(),
            calldata_hex: "abcd".to_string(),
        }
    }

    #[test]
    fn quote_log_schema_has_expected_field_count() {
        let schema = quote_log_schema();
        assert_eq!(schema.fields().len(), 17);
    }

    #[test]
    fn quote_log_round_trip() {
        use arrow::array::{BooleanArray, UInt64Array};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quote_log.parquet");

        let records = vec![sample_quote_log_record("q1"), {
            let mut r = sample_quote_log_record("q2");
            r.is_winner = false;
            r.gap_to_second_best_bps = None;
            r.slippage_tolerance = Some(0.005);
            r
        }];

        write_quote_log_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 17);

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

        // token_in / token_out columns
        let token_in_col = batch
            .column_by_name("token_in")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .unwrap();
        assert_eq!(token_in_col.value(0), "0x0101010101010101010101010101010101010101");
        let token_out_col = batch
            .column_by_name("token_out")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .unwrap();
        assert_eq!(token_out_col.value(0), "0x0202020202020202020202020202020202020202");
    }

    #[test]
    fn empty_quote_log_produces_valid_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty_quote.parquet");

        write_quote_log_parquet(&path, &[]).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert_eq!(builder.schema().fields().len(), 17);

        let reader = builder.build().unwrap();
        let total_rows: usize = reader
            .filter_map(|b| b.ok())
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total_rows, 0);
    }

    // -----------------------------------------------------------------------
    // Route decay parquet tests
    // -----------------------------------------------------------------------

    fn sample_route_decay_record(
        quote_id: &str,
        block_offset: u32,
        success: bool,
    ) -> RouteDecayRecord {
        RouteDecayRecord {
            quote_id: quote_id.to_string(),
            solver_id: "solver-a".to_string(),
            request_id: "req-1".to_string(),
            block_offset,
            eth_call_amount_out: "990".to_string(),
            eth_call_gas_used: 150_000,
            eth_call_success: success,
            eth_call_revert_reason: if success {
                None
            } else {
                Some("execution reverted".to_string())
            },
            eth_call_decay_bps: 10.0,
        }
    }

    #[test]
    fn route_decay_schema_has_expected_field_count() {
        let schema = route_decay_schema();
        assert_eq!(schema.fields().len(), 9);
    }

    #[test]
    fn route_decay_round_trip() {
        use arrow::array::{BooleanArray, UInt64Array};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("route_decay.parquet");

        let records = vec![
            sample_route_decay_record("q1", 1, true),
            sample_route_decay_record("q1", 2, false),
            sample_route_decay_record("q2", 1, true),
        ];

        write_route_decay_parquet(&path, &records).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let mut reader = builder.build().unwrap();

        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 9);

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
        assert_eq!(offsets.value(1), 2);

        let success = batch
            .column(6)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(success.value(0));
        assert!(!success.value(1));

        let revert = batch
            .column(7)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(revert.is_null(0));
        assert_eq!(revert.value(1), "execution reverted");

        let gas = batch
            .column(5)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(gas.value(0), 150_000);
    }

    #[test]
    fn empty_route_decay_produces_valid_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("empty_route_decay.parquet");

        write_route_decay_parquet(&path, &[]).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert_eq!(builder.schema().fields().len(), 9);

        let reader = builder.build().unwrap();
        let total_rows: usize = reader
            .filter_map(|b| b.ok())
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(total_rows, 0);
    }
}
