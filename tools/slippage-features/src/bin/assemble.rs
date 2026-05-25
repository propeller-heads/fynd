//! Feature assembly binary: joins quote log, hop decay, and route decay
//! parquet sources into a unified analysis dataset with computed features.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use arrow::{
    array::{
        Array, ArrayRef, BooleanBuilder, Float64Builder, StringBuilder, UInt32Builder,
        UInt64Builder,
    },
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use clap::Parser;
use parquet::{
    arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter},
    basic::Compression,
    file::properties::WriterProperties,
};
use slippage_features::coingecko::{CoinGeckoClient, PairClassification};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "assemble", about = "Join decay parquets + features into unified dataset")]
struct Args {
    /// Path to quote log parquet directory
    #[arg(long)]
    quote_log_dir: PathBuf,

    /// Path to hop decay parquet directory (from Tycho resim)
    #[arg(long)]
    hop_decay_dir: PathBuf,

    /// Path to Tycho route decay parquet directory (from Tycho resim)
    #[arg(long)]
    tycho_route_decay_dir: PathBuf,

    /// Path to node resim route decay parquet directory
    #[arg(long)]
    route_decay_dir: PathBuf,

    /// Output directory for unified dataset
    #[arg(long)]
    output_dir: PathBuf,

    /// CoinGecko API key for token classification
    #[arg(long, env = "COINGECKO_API_KEY", default_value = "")]
    coingecko_api_key: String,

    /// Block gap threshold for temporal gap detection (default 100)
    #[arg(long, default_value_t = 100)]
    gap_threshold: u64,
}

// ---------------------------------------------------------------------------
// Parsed records from each parquet source
// ---------------------------------------------------------------------------

struct QuoteLogRow {
    quote_id: String,
    solver_id: String,
    request_id: String,
    is_winner: bool,
    block_number: u64,
    chain_id: u64,
    amount_in: String,
    amount_out: String,
    gas_estimate: u64,
    algorithm_type: String,
    n_alternatives: u32,
    gap_to_second_best_bps: Option<f64>,
    token_in: String,
    token_out: String,
    route_json: String,
}

struct HopDecayRow {
    quote_id: String,
    solver_id: String,
    block_offset: u32,
    hop_decay_bps: f64,
}

struct TychoRouteDecayRow {
    quote_id: String,
    solver_id: String,
    block_offset: u32,
    route_decay_bps: f64,
    market_movement_bps: Option<f64>,
    execution_slippage_bps: Option<f64>,
}

struct RouteDecayRow {
    quote_id: String,
    solver_id: String,
    block_offset: u32,
    eth_call_amount_out: String,
    eth_call_gas_used: u64,
    eth_call_success: bool,
    eth_call_decay_bps: f64,
}

// ---------------------------------------------------------------------------
// Unified output record
// ---------------------------------------------------------------------------

struct UnifiedRecord {
    // Join keys
    quote_id: String,
    solver_id: String,
    request_id: String,

    // From quote log
    is_winner: bool,
    block_number: u64,
    chain_id: u64,
    amount_in: String,
    amount_out: String,
    gas_estimate: u64,
    algorithm_type: String,
    n_alternatives: u32,
    gap_to_second_best_bps: Option<f64>,

    // Token addresses
    token_in: String,
    token_out: String,

    // Token/pair classification (from CoinGecko)
    token_in_category: String,
    token_out_category: String,
    pair_bucket: String,
    log_mcap_ratio: Option<f64>,
    min_mcap: Option<f64>,
    max_mcap: Option<f64>,

    // Decay (aggregated per block_offset)
    block_offset: u32,
    max_hop_decay_bps: f64,
    route_decay_bps: f64,
    market_movement_bps: Option<f64>,
    execution_slippage_bps: Option<f64>,

    // Route decay (from node resim)
    eth_call_amount_out: Option<String>,
    eth_call_gas_used: Option<u64>,
    eth_call_success: Option<bool>,
    eth_call_decay_bps: Option<f64>,

    // Computed: route topology
    hop_count: u32,
    split_count: u32,

    // Computed: chain/env
    is_l2: bool,

    // Computed: temporal
    hour_of_day: Option<u32>,
    day_of_week: Option<u32>,
}

// ---------------------------------------------------------------------------
// Parquet readers
// ---------------------------------------------------------------------------

fn read_parquet_files_from_dir(dir: &std::path::Path) -> anyhow::Result<Vec<RecordBatch>> {
    let mut batches = Vec::new();

    let entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "parquet")
        })
        .collect();

    if entries.is_empty() {
        warn!(dir = %dir.display(), "no parquet files found");
        return Ok(batches);
    }

    for entry in &entries {
        let path = entry.path();
        info!(path = %path.display(), "reading parquet file");

        let file = std::fs::File::open(&path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let reader = builder.build()?;

        for batch_result in reader {
            batches.push(batch_result?);
        }
    }

    Ok(batches)
}

fn parse_quote_log_rows(batches: &[RecordBatch]) -> Vec<QuoteLogRow> {
    use arrow::array::AsArray;

    let mut rows = Vec::new();

    for batch in batches {
        let quote_ids = batch
            .column_by_name("quote_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let solver_ids = batch
            .column_by_name("solver_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let request_ids = batch
            .column_by_name("request_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let block_numbers = batch
            .column_by_name("block_number")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt64Type>());
        let chain_ids = batch
            .column_by_name("chain_id")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt64Type>());
        let amounts_in = batch
            .column_by_name("amount_in")
            .and_then(|c| c.as_string_opt::<i32>());
        let amounts_out = batch
            .column_by_name("amount_out")
            .and_then(|c| c.as_string_opt::<i32>());
        let gas_estimates = batch
            .column_by_name("gas_estimate")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt64Type>());
        let algorithm_types = batch
            .column_by_name("algorithm_type")
            .and_then(|c| c.as_string_opt::<i32>());
        let is_winners = batch
            .column_by_name("is_winner")
            .and_then(|c| c.as_boolean_opt());
        let n_alternatives_arr = batch
            .column_by_name("n_alternatives")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt32Type>());
        let gap_to_second_best_arr = batch
            .column_by_name("gap_to_second_best_bps")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>());
        let token_ins = batch
            .column_by_name("token_in")
            .and_then(|c| c.as_string_opt::<i32>());
        let token_outs = batch
            .column_by_name("token_out")
            .and_then(|c| c.as_string_opt::<i32>());
        let route_jsons = batch
            .column_by_name("route_json")
            .and_then(|c| c.as_string_opt::<i32>());

        let (
            Some(quote_ids),
            Some(solver_ids),
            Some(request_ids),
            Some(block_numbers),
            Some(chain_ids),
            Some(amounts_in),
            Some(amounts_out),
            Some(gas_estimates),
            Some(algorithm_types),
            Some(route_jsons),
        ) = (
            quote_ids,
            solver_ids,
            request_ids,
            block_numbers,
            chain_ids,
            amounts_in,
            amounts_out,
            gas_estimates,
            algorithm_types,
            route_jsons,
        )
        else {
            warn!("missing expected columns in quote log batch, skipping");
            continue;
        };

        for row in 0..batch.num_rows() {
            // token_in/token_out may be absent in older parquets;
            // fall back to extracting from the route JSON.
            let token_in = token_ins
                .map(|t| t.value(row).to_string())
                .unwrap_or_else(|| extract_token_from_route(route_jsons.value(row), true));
            let token_out = token_outs
                .map(|t| t.value(row).to_string())
                .unwrap_or_else(|| extract_token_from_route(route_jsons.value(row), false));

            let is_winner = is_winners
                .map(|a| a.value(row))
                .unwrap_or(false);
            let n_alts = n_alternatives_arr
                .map(|a| a.value(row))
                .unwrap_or(0);
            let gap = gap_to_second_best_arr.and_then(|a| {
                if a.is_null(row) { None } else { Some(a.value(row)) }
            });

            rows.push(QuoteLogRow {
                quote_id: quote_ids.value(row).to_string(),
                solver_id: solver_ids.value(row).to_string(),
                request_id: request_ids.value(row).to_string(),
                is_winner,
                block_number: block_numbers.value(row),
                chain_id: chain_ids.value(row),
                amount_in: amounts_in.value(row).to_string(),
                amount_out: amounts_out.value(row).to_string(),
                gas_estimate: gas_estimates.value(row),
                algorithm_type: algorithm_types.value(row).to_string(),
                n_alternatives: n_alts,
                gap_to_second_best_bps: gap,
                token_in,
                token_out,
                route_json: route_jsons.value(row).to_string(),
            });
        }
    }

    rows
}

fn parse_hop_decay_rows(batches: &[RecordBatch]) -> Vec<HopDecayRow> {
    use arrow::array::AsArray;

    let mut rows = Vec::new();

    for batch in batches {
        let quote_ids = batch
            .column_by_name("quote_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let solver_ids = batch
            .column_by_name("solver_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let block_offsets = batch
            .column_by_name("block_offset")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt32Type>());
        let hop_decay_bps_arr = batch
            .column_by_name("hop_decay_bps")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>());

        let (
            Some(quote_ids),
            Some(solver_ids),
            Some(block_offsets),
            Some(hop_decay_bps_arr),
        ) = (quote_ids, solver_ids, block_offsets, hop_decay_bps_arr)
        else {
            warn!("missing expected columns in hop decay batch, skipping");
            continue;
        };

        for row in 0..batch.num_rows() {
            rows.push(HopDecayRow {
                quote_id: quote_ids.value(row).to_string(),
                solver_id: solver_ids.value(row).to_string(),
                block_offset: block_offsets.value(row),
                hop_decay_bps: hop_decay_bps_arr.value(row),
            });
        }
    }

    rows
}

fn parse_tycho_route_decay_rows(batches: &[RecordBatch]) -> Vec<TychoRouteDecayRow> {
    use arrow::array::AsArray;

    let mut rows = Vec::new();

    for batch in batches {
        let quote_ids = batch
            .column_by_name("quote_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let solver_ids = batch
            .column_by_name("solver_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let block_offsets = batch
            .column_by_name("block_offset")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt32Type>());
        let route_decay_bps_arr = batch
            .column_by_name("route_decay_bps")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>());
        let market_movement_bps_arr = batch
            .column_by_name("market_movement_bps")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>());
        let execution_slippage_bps_arr = batch
            .column_by_name("execution_slippage_bps")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>());

        let (
            Some(quote_ids),
            Some(solver_ids),
            Some(block_offsets),
            Some(route_decay_bps_arr),
            Some(market_movement_bps_arr),
            Some(execution_slippage_bps_arr),
        ) = (
            quote_ids,
            solver_ids,
            block_offsets,
            route_decay_bps_arr,
            market_movement_bps_arr,
            execution_slippage_bps_arr,
        )
        else {
            warn!("missing expected columns in tycho route decay batch, skipping");
            continue;
        };

        for row in 0..batch.num_rows() {
            let mm = if market_movement_bps_arr.is_null(row) {
                None
            } else {
                Some(market_movement_bps_arr.value(row))
            };
            let es = if execution_slippage_bps_arr.is_null(row) {
                None
            } else {
                Some(execution_slippage_bps_arr.value(row))
            };
            rows.push(TychoRouteDecayRow {
                quote_id: quote_ids.value(row).to_string(),
                solver_id: solver_ids.value(row).to_string(),
                block_offset: block_offsets.value(row),
                route_decay_bps: route_decay_bps_arr.value(row),
                market_movement_bps: mm,
                execution_slippage_bps: es,
            });
        }
    }

    rows
}

fn parse_route_decay_rows(batches: &[RecordBatch]) -> Vec<RouteDecayRow> {
    use arrow::array::AsArray;

    let mut rows = Vec::new();

    for batch in batches {
        let quote_ids = batch
            .column_by_name("quote_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let solver_ids = batch
            .column_by_name("solver_id")
            .and_then(|c| c.as_string_opt::<i32>());
        let block_offsets = batch
            .column_by_name("block_offset")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt32Type>());
        let eth_call_amounts_out = batch
            .column_by_name("eth_call_amount_out")
            .and_then(|c| c.as_string_opt::<i32>());
        let eth_call_gas_used_arr = batch
            .column_by_name("eth_call_gas_used")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt64Type>());
        let eth_call_success_arr = batch
            .column_by_name("eth_call_success")
            .and_then(|c| c.as_boolean_opt());
        let eth_call_decay_bps_arr = batch
            .column_by_name("eth_call_decay_bps")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::Float64Type>());

        let (
            Some(quote_ids),
            Some(solver_ids),
            Some(block_offsets),
            Some(eth_call_amounts_out),
            Some(eth_call_gas_used_arr),
            Some(eth_call_success_arr),
            Some(eth_call_decay_bps_arr),
        ) = (
            quote_ids,
            solver_ids,
            block_offsets,
            eth_call_amounts_out,
            eth_call_gas_used_arr,
            eth_call_success_arr,
            eth_call_decay_bps_arr,
        )
        else {
            warn!("missing expected columns in route decay batch, skipping");
            continue;
        };

        for row in 0..batch.num_rows() {
            rows.push(RouteDecayRow {
                quote_id: quote_ids.value(row).to_string(),
                solver_id: solver_ids.value(row).to_string(),
                block_offset: block_offsets.value(row),
                eth_call_amount_out: eth_call_amounts_out
                    .value(row)
                    .to_string(),
                eth_call_gas_used: eth_call_gas_used_arr.value(row),
                eth_call_success: eth_call_success_arr.value(row),
                eth_call_decay_bps: eth_call_decay_bps_arr.value(row),
            });
        }
    }

    rows
}

// ---------------------------------------------------------------------------
// Feature computation
// ---------------------------------------------------------------------------

/// Extract token_in (first swap) or token_out (last swap) from route JSON.
fn extract_token_from_route(route_json: &str, is_input: bool) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(route_json);
    let Ok(value) = parsed else {
        return String::new();
    };

    let Some(swaps) = value
        .get("swaps")
        .and_then(|s| s.as_array())
    else {
        return String::new();
    };

    let swap = if is_input { swaps.first() } else { swaps.last() };

    let field = if is_input { "token_in" } else { "token_out" };

    swap.and_then(|s| s.get(field))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract hop_count and split_count from route JSON.
///
/// hop_count = number of swaps in the route.
/// split_count = number of swaps with a non-zero split ratio, indicating
/// parallel route legs.
fn compute_route_topology(route_json: &str) -> (u32, u32) {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(route_json);
    let Ok(value) = parsed else {
        return (0, 0);
    };

    let Some(swaps) = value
        .get("swaps")
        .and_then(|s| s.as_array())
    else {
        return (0, 0);
    };

    let hop_count = swaps.len() as u32;
    let mut split_count: u32 = 0;
    for swap in swaps {
        if let Some(split) = swap
            .get("split")
            .and_then(|s| s.as_f64())
        {
            if split > 0.0 && split < 1.0 {
                split_count += 1;
            }
        }
    }

    (hop_count, split_count)
}

const BASE_CHAIN_ID: u64 = 8453;

/// Approximate hour-of-day and day-of-week from block number.
///
/// Uses a reference block to estimate the timestamp. This is a rough
/// approximation (Ethereum mainnet ~12s/block). Returns None if the
/// block number precedes our reference point.
fn estimate_temporal_features(block_number: u64, chain_id: u64) -> (Option<u32>, Option<u32>) {
    // Ethereum mainnet reference: block 17_000_000 ~ 2023-04-12 22:00 UTC
    // Base reference: block 10_000_000 ~ 2024-04-01 (2s/block)
    let (ref_block, ref_timestamp, block_time_secs) = if chain_id == BASE_CHAIN_ID {
        (10_000_000_u64, 1_711_929_600_i64, 2_i64)
    } else {
        (17_000_000_u64, 1_681_340_400_i64, 12_i64)
    };

    if block_number < ref_block {
        return (None, None);
    }

    let delta_blocks = block_number - ref_block;
    let estimated_ts = ref_timestamp + (delta_blocks as i64 * block_time_secs);

    let dt = chrono::DateTime::from_timestamp(estimated_ts, 0);
    let Some(dt) = dt else {
        return (None, None);
    };

    use chrono::{Datelike as _, Timelike as _};

    let hour = dt.hour();
    let weekday = dt.weekday().num_days_from_monday(); // 0=Mon, 6=Sun

    (Some(hour), Some(weekday))
}

// ---------------------------------------------------------------------------
// Join logic
// ---------------------------------------------------------------------------

/// Join the data sources by (quote_id, solver_id) and produce unified
/// records with computed features.
fn join_datasets(
    quote_logs: &[QuoteLogRow],
    hop_decays: &[HopDecayRow],
    tycho_route_decays: &[TychoRouteDecayRow],
    route_decays: &[RouteDecayRow],
    pair_classifications: &HashMap<(String, String), PairClassification>,
    token_categories: &HashMap<String, String>,
) -> Vec<UnifiedRecord> {
    let mut quote_map: HashMap<(String, String), &QuoteLogRow> = HashMap::new();
    for ql in quote_logs {
        quote_map.insert((ql.quote_id.clone(), ql.solver_id.clone()), ql);
    }

    // Aggregate hop decays: max_hop_decay_bps per (quote, solver, offset).
    let mut hop_map: HashMap<(String, String, u32), f64> = HashMap::new();
    for hd in hop_decays {
        let key = (hd.quote_id.clone(), hd.solver_id.clone(), hd.block_offset);
        let entry = hop_map.entry(key).or_insert(f64::NEG_INFINITY);
        if hd.hop_decay_bps > *entry {
            *entry = hd.hop_decay_bps;
        }
    }

    // Index Tycho route decays by (quote_id, solver_id, block_offset).
    let mut tycho_map: HashMap<(String, String, u32), &TychoRouteDecayRow> = HashMap::new();
    for trd in tycho_route_decays {
        tycho_map.insert(
            (trd.quote_id.clone(), trd.solver_id.clone(), trd.block_offset),
            trd,
        );
    }

    // Index node resim route decays.
    let mut route_map: HashMap<(String, String, u32), &RouteDecayRow> = HashMap::new();
    for rd in route_decays {
        route_map.insert((rd.quote_id.clone(), rd.solver_id.clone(), rd.block_offset), rd);
    }

    // Collect all unique keys from both hop and route sources.
    let mut all_keys: Vec<(String, String, u32)> = Vec::new();
    for key in hop_map.keys() {
        all_keys.push(key.clone());
    }
    for key in tycho_map.keys() {
        if !hop_map.contains_key(key) {
            all_keys.push(key.clone());
        }
    }
    for key in route_map.keys() {
        if !hop_map.contains_key(key) && !tycho_map.contains_key(key) {
            all_keys.push(key.clone());
        }
    }
    all_keys.sort();

    let mut records = Vec::with_capacity(all_keys.len());

    for (quote_id, solver_id, block_offset) in &all_keys {
        let ql_key = (quote_id.clone(), solver_id.clone());
        let Some(ql) = quote_map.get(&ql_key) else {
            warn!(
                quote_id = %quote_id,
                solver_id = %solver_id,
                "decay row has no matching quote log entry, skipping"
            );
            continue;
        };

        let full_key = (quote_id.clone(), solver_id.clone(), *block_offset);
        let max_hop_decay = hop_map.get(&full_key);
        let tycho_rd = tycho_map.get(&full_key);
        let node_rd = route_map.get(&full_key);

        let (hop_count, split_count) = compute_route_topology(&ql.route_json);
        let is_l2 = ql.chain_id == BASE_CHAIN_ID;
        let (hour_of_day, day_of_week) = estimate_temporal_features(ql.block_number, ql.chain_id);

        let pair_key = (ql.token_in.clone(), ql.token_out.clone());
        let pair = pair_classifications.get(&pair_key);
        let token_in_category = token_categories
            .get(&ql.token_in)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let token_out_category = token_categories
            .get(&ql.token_out)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        records.push(UnifiedRecord {
            quote_id: quote_id.clone(),
            solver_id: solver_id.clone(),
            request_id: ql.request_id.clone(),
            is_winner: ql.is_winner,
            block_number: ql.block_number,
            chain_id: ql.chain_id,
            amount_in: ql.amount_in.clone(),
            amount_out: ql.amount_out.clone(),
            gas_estimate: ql.gas_estimate,
            algorithm_type: ql.algorithm_type.clone(),
            n_alternatives: ql.n_alternatives,
            gap_to_second_best_bps: ql.gap_to_second_best_bps,
            token_in: ql.token_in.clone(),
            token_out: ql.token_out.clone(),
            token_in_category,
            token_out_category,
            pair_bucket: pair.map_or_else(|| "unknown".to_string(), |p| p.bucket.clone()),
            log_mcap_ratio: pair.and_then(|p| p.log_mcap_ratio),
            min_mcap: pair.and_then(|p| p.min_mcap),
            max_mcap: pair.and_then(|p| p.max_mcap),
            block_offset: *block_offset,
            max_hop_decay_bps: max_hop_decay.copied().unwrap_or(0.0),
            route_decay_bps: tycho_rd.map_or(0.0, |t| t.route_decay_bps),
            market_movement_bps: tycho_rd.and_then(|t| t.market_movement_bps),
            execution_slippage_bps: tycho_rd.and_then(|t| t.execution_slippage_bps),
            eth_call_amount_out: node_rd.map(|r| r.eth_call_amount_out.clone()),
            eth_call_gas_used: node_rd.map(|r| r.eth_call_gas_used),
            eth_call_success: node_rd.map(|r| r.eth_call_success),
            eth_call_decay_bps: node_rd.map(|r| r.eth_call_decay_bps),
            hop_count,
            split_count,
            is_l2,
            hour_of_day,
            day_of_week,
        });
    }

    records
}

// ---------------------------------------------------------------------------
// Unified parquet output
// ---------------------------------------------------------------------------

fn unified_schema() -> Schema {
    Schema::new(vec![
        // Join keys
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("request_id", DataType::Utf8, false),
        // Quote log
        Field::new("is_winner", DataType::Boolean, false),
        Field::new("block_number", DataType::UInt64, false),
        Field::new("chain_id", DataType::UInt64, false),
        Field::new("amount_in", DataType::Utf8, false),
        Field::new("amount_out", DataType::Utf8, false),
        Field::new("gas_estimate", DataType::UInt64, false),
        Field::new("algorithm_type", DataType::Utf8, false),
        Field::new("n_alternatives", DataType::UInt32, false),
        Field::new("gap_to_second_best_bps", DataType::Float64, true),
        // Token addresses
        Field::new("token_in", DataType::Utf8, false),
        Field::new("token_out", DataType::Utf8, false),
        // Token/pair classification (CoinGecko)
        Field::new("token_in_category", DataType::Utf8, false),
        Field::new("token_out_category", DataType::Utf8, false),
        Field::new("pair_bucket", DataType::Utf8, false),
        Field::new("log_mcap_ratio", DataType::Float64, true),
        Field::new("min_mcap", DataType::Float64, true),
        Field::new("max_mcap", DataType::Float64, true),
        // Decay (aggregated)
        Field::new("block_offset", DataType::UInt32, false),
        Field::new("max_hop_decay_bps", DataType::Float64, false),
        Field::new("route_decay_bps", DataType::Float64, false),
        Field::new("market_movement_bps", DataType::Float64, true),
        Field::new("execution_slippage_bps", DataType::Float64, true),
        // Route decay (nullable — may not have node resim data)
        Field::new("eth_call_amount_out", DataType::Utf8, true),
        Field::new("eth_call_gas_used", DataType::UInt64, true),
        Field::new("eth_call_success", DataType::Boolean, true),
        Field::new("eth_call_decay_bps", DataType::Float64, true),
        // Computed: topology
        Field::new("hop_count", DataType::UInt32, false),
        Field::new("split_count", DataType::UInt32, false),
        // Computed: chain/env
        Field::new("is_l2", DataType::Boolean, false),
        // Computed: temporal
        Field::new("hour_of_day", DataType::UInt32, true),
        Field::new("day_of_week", DataType::UInt32, true),
    ])
}

fn write_unified_parquet(
    output_dir: &std::path::Path,
    records: &[UnifiedRecord],
) -> anyhow::Result<()> {
    // Partition by chain_id: group records, write one file per chain.
    let mut by_chain: HashMap<u64, Vec<&UnifiedRecord>> = HashMap::new();
    for record in records {
        by_chain
            .entry(record.chain_id)
            .or_default()
            .push(record);
    }

    for (chain_id, chain_records) in &by_chain {
        let chain_dir = output_dir.join(format!("chain_id={chain_id}"));
        std::fs::create_dir_all(&chain_dir)?;

        let path = chain_dir.join("unified.parquet");
        write_unified_parquet_file(&path, chain_records)?;

        info!(
            chain_id = chain_id,
            records = chain_records.len(),
            path = %path.display(),
            "wrote unified parquet partition"
        );
    }

    Ok(())
}

fn write_unified_parquet_file(
    path: &std::path::Path,
    records: &[&UnifiedRecord],
) -> anyhow::Result<()> {
    let schema = Arc::new(unified_schema());
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
    let mut token_in = StringBuilder::with_capacity(n, n * 42);
    let mut token_out = StringBuilder::with_capacity(n, n * 42);
    let mut token_in_category = StringBuilder::with_capacity(n, n * 12);
    let mut token_out_category = StringBuilder::with_capacity(n, n * 12);
    let mut pair_bucket = StringBuilder::with_capacity(n, n * 20);
    let mut log_mcap_ratio = Float64Builder::with_capacity(n);
    let mut min_mcap = Float64Builder::with_capacity(n);
    let mut max_mcap = Float64Builder::with_capacity(n);
    let mut block_offset = UInt32Builder::with_capacity(n);
    let mut max_hop_decay_bps = Float64Builder::with_capacity(n);
    let mut route_decay_bps = Float64Builder::with_capacity(n);
    let mut market_movement_bps = Float64Builder::with_capacity(n);
    let mut execution_slippage_bps = Float64Builder::with_capacity(n);
    let mut eth_call_amount_out = StringBuilder::with_capacity(n, n * 32);
    let mut eth_call_gas_used = UInt64Builder::with_capacity(n);
    let mut eth_call_success = BooleanBuilder::with_capacity(n);
    let mut eth_call_decay_bps = Float64Builder::with_capacity(n);
    let mut hop_count = UInt32Builder::with_capacity(n);
    let mut split_count = UInt32Builder::with_capacity(n);
    let mut is_l2 = BooleanBuilder::with_capacity(n);
    let mut hour_of_day = UInt32Builder::with_capacity(n);
    let mut day_of_week = UInt32Builder::with_capacity(n);

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
        token_in.append_value(&r.token_in);
        token_out.append_value(&r.token_out);
        token_in_category.append_value(&r.token_in_category);
        token_out_category.append_value(&r.token_out_category);
        pair_bucket.append_value(&r.pair_bucket);

        match r.log_mcap_ratio {
            Some(v) => log_mcap_ratio.append_value(v),
            None => log_mcap_ratio.append_null(),
        }
        match r.min_mcap {
            Some(v) => min_mcap.append_value(v),
            None => min_mcap.append_null(),
        }
        match r.max_mcap {
            Some(v) => max_mcap.append_value(v),
            None => max_mcap.append_null(),
        }

        block_offset.append_value(r.block_offset);
        max_hop_decay_bps.append_value(r.max_hop_decay_bps);
        route_decay_bps.append_value(r.route_decay_bps);
        match r.market_movement_bps {
            Some(v) => market_movement_bps.append_value(v),
            None => market_movement_bps.append_null(),
        }
        match r.execution_slippage_bps {
            Some(v) => execution_slippage_bps.append_value(v),
            None => execution_slippage_bps.append_null(),
        }

        match &r.eth_call_amount_out {
            Some(v) => eth_call_amount_out.append_value(v),
            None => eth_call_amount_out.append_null(),
        }
        match r.eth_call_gas_used {
            Some(v) => eth_call_gas_used.append_value(v),
            None => eth_call_gas_used.append_null(),
        }
        match r.eth_call_success {
            Some(v) => eth_call_success.append_value(v),
            None => eth_call_success.append_null(),
        }
        match r.eth_call_decay_bps {
            Some(v) => eth_call_decay_bps.append_value(v),
            None => eth_call_decay_bps.append_null(),
        }

        hop_count.append_value(r.hop_count);
        split_count.append_value(r.split_count);
        is_l2.append_value(r.is_l2);

        match r.hour_of_day {
            Some(v) => hour_of_day.append_value(v),
            None => hour_of_day.append_null(),
        }
        match r.day_of_week {
            Some(v) => day_of_week.append_value(v),
            None => day_of_week.append_null(),
        }
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
        Arc::new(token_in.finish()),
        Arc::new(token_out.finish()),
        Arc::new(token_in_category.finish()),
        Arc::new(token_out_category.finish()),
        Arc::new(pair_bucket.finish()),
        Arc::new(log_mcap_ratio.finish()),
        Arc::new(min_mcap.finish()),
        Arc::new(max_mcap.finish()),
        Arc::new(block_offset.finish()),
        Arc::new(max_hop_decay_bps.finish()),
        Arc::new(route_decay_bps.finish()),
        Arc::new(market_movement_bps.finish()),
        Arc::new(execution_slippage_bps.finish()),
        Arc::new(eth_call_amount_out.finish()),
        Arc::new(eth_call_gas_used.finish()),
        Arc::new(eth_call_success.finish()),
        Arc::new(eth_call_decay_bps.finish()),
        Arc::new(hop_count.finish()),
        Arc::new(split_count.finish()),
        Arc::new(is_l2.finish()),
        Arc::new(hour_of_day.finish()),
        Arc::new(day_of_week.finish()),
    ];

    let batch = RecordBatch::try_new(schema.clone(), columns)?;

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        .build();

    let file = std::fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
    writer.write(&batch)?;
    writer.close()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Temporal gap detection
// ---------------------------------------------------------------------------

/// Detect temporal gaps in block coverage from quote log data.
///
/// Sorts all observed block numbers and reports consecutive gaps larger than
/// `threshold`. This helps identify collection outages.
fn detect_block_gaps(quote_logs: &[QuoteLogRow], threshold: u64) {
    if quote_logs.is_empty() {
        return;
    }

    let mut blocks: Vec<u64> = quote_logs
        .iter()
        .map(|q| q.block_number)
        .collect();
    blocks.sort_unstable();
    blocks.dedup();

    if blocks.len() < 2 {
        info!(blocks = blocks.len(), "only one unique block observed, no gap detection possible");
        return;
    }

    let first = blocks[0];
    let last = blocks[blocks.len() - 1];
    info!(
        first_block = first,
        last_block = last,
        unique_blocks = blocks.len(),
        "scanning for temporal gaps (threshold = {threshold} blocks)"
    );

    let mut gap_count = 0_u32;
    for pair in blocks.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > threshold {
            let approx_minutes = gap * 12 / 60;
            info!(
                "Gap detected: blocks {} to {} ({} blocks, ~{} minutes)",
                pair[0], pair[1], gap, approx_minutes
            );
            gap_count += 1;
        }
    }

    if gap_count == 0 {
        info!("no temporal gaps detected above threshold of {threshold} blocks");
    } else {
        info!("{gap_count} temporal gap(s) detected");
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!(
        quote_log_dir = %args.quote_log_dir.display(),
        hop_decay_dir = %args.hop_decay_dir.display(),
        tycho_route_decay_dir = %args.tycho_route_decay_dir.display(),
        route_decay_dir = %args.route_decay_dir.display(),
        output_dir = %args.output_dir.display(),
        "starting feature assembly"
    );

    let quote_log_batches = read_parquet_files_from_dir(&args.quote_log_dir)?;
    let hop_decay_batches = read_parquet_files_from_dir(&args.hop_decay_dir)?;
    let tycho_route_decay_batches = read_parquet_files_from_dir(&args.tycho_route_decay_dir)?;
    let route_decay_batches = read_parquet_files_from_dir(&args.route_decay_dir)?;

    let quote_logs = parse_quote_log_rows(&quote_log_batches);
    let hop_decays = parse_hop_decay_rows(&hop_decay_batches);
    let tycho_route_decays = parse_tycho_route_decay_rows(&tycho_route_decay_batches);
    let route_decays = parse_route_decay_rows(&route_decay_batches);

    info!(
        quote_logs = quote_logs.len(),
        hop_decays = hop_decays.len(),
        tycho_route_decays = tycho_route_decays.len(),
        route_decays = route_decays.len(),
        "loaded source data"
    );

    if quote_logs.is_empty() {
        info!("no quote logs found, nothing to assemble");
        return Ok(());
    }

    detect_block_gaps(&quote_logs, args.gap_threshold);

    // Collect unique token addresses for CoinGecko lookup
    let mut unique_tokens: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ql in &quote_logs {
        if !ql.token_in.is_empty() {
            unique_tokens.insert(ql.token_in.clone());
        }
        if !ql.token_out.is_empty() {
            unique_tokens.insert(ql.token_out.clone());
        }
    }

    // Fetch CoinGecko metadata for all unique tokens
    let mut token_categories: HashMap<String, String> = HashMap::new();
    let mut pair_classifications: HashMap<(String, String), PairClassification> = HashMap::new();

    if !args.coingecko_api_key.is_empty() {
        let mut cg_client = CoinGeckoClient::new(args.coingecko_api_key.clone());

        info!(tokens = unique_tokens.len(), "fetching CoinGecko metadata for unique tokens");

        for address in &unique_tokens {
            match cg_client
                .get_token_metadata(address)
                .await
            {
                Ok(meta) => {
                    token_categories.insert(address.clone(), meta.category.as_str().to_string());
                }
                Err(e) => {
                    warn!(
                        address = %address,
                        error = %e,
                        "failed to fetch CoinGecko metadata, using unknown"
                    );
                    token_categories.insert(address.clone(), "unknown".to_string());
                }
            }
        }

        // Classify all unique (token_in, token_out) pairs
        let mut unique_pairs: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for ql in &quote_logs {
            if !ql.token_in.is_empty() && !ql.token_out.is_empty() {
                unique_pairs.insert((ql.token_in.clone(), ql.token_out.clone()));
            }
        }

        for (tin, tout) in &unique_pairs {
            // Clone metadata to avoid holding borrows across calls.
            let meta_in = cg_client
                .get_token_metadata(tin)
                .await
                .ok()
                .cloned();
            let meta_out = cg_client
                .get_token_metadata(tout)
                .await
                .ok()
                .cloned();
            if let (Some(a), Some(b)) = (meta_in, meta_out) {
                let classification = cg_client.classify_pair(&a, &b);
                pair_classifications.insert((tin.clone(), tout.clone()), classification);
            }
        }

        info!(
            tokens = token_categories.len(),
            pairs = pair_classifications.len(),
            "CoinGecko classification complete"
        );
    } else {
        info!("no CoinGecko API key, skipping token classification");
    }

    let unified = join_datasets(
        &quote_logs,
        &hop_decays,
        &tycho_route_decays,
        &route_decays,
        &pair_classifications,
        &token_categories,
    );

    info!(records = unified.len(), "assembled unified dataset");

    // Write output partitioned by chain_id
    std::fs::create_dir_all(&args.output_dir)?;
    write_unified_parquet(&args.output_dir, &unified)?;

    info!("feature assembly complete");

    Ok(())
}

#[cfg(test)]
mod tests {
    use slippage_features::parquet_writer::{
        write_hop_decay_parquet, write_quote_log_parquet, write_route_decay_parquet,
        write_tycho_route_decay_parquet, HopDecayRecord, QuoteLogRecord, RouteDecayRecord,
        TychoRouteDecayRecord,
    };

    use super::*;

    #[test]
    fn cli_parsing() {
        let args = Args::parse_from([
            "assemble",
            "--quote-log-dir",
            "/tmp/quotes",
            "--hop-decay-dir",
            "/tmp/hops",
            "--tycho-route-decay-dir",
            "/tmp/tycho",
            "--route-decay-dir",
            "/tmp/routes",
            "--output-dir",
            "/tmp/out",
        ]);
        assert_eq!(args.quote_log_dir, PathBuf::from("/tmp/quotes"));
        assert_eq!(args.hop_decay_dir, PathBuf::from("/tmp/hops"));
        assert_eq!(args.tycho_route_decay_dir, PathBuf::from("/tmp/tycho"));
        assert_eq!(args.route_decay_dir, PathBuf::from("/tmp/routes"));
        assert_eq!(args.output_dir, PathBuf::from("/tmp/out"));
        assert_eq!(args.coingecko_api_key, "");
        assert_eq!(args.gap_threshold, 100);
    }

    #[test]
    fn route_topology_empty_json() {
        let (hops, splits) = compute_route_topology("{}");
        assert_eq!(hops, 0);
        assert_eq!(splits, 0);
    }

    #[test]
    fn route_topology_single_hop() {
        let json = r#"{"swaps":[{"component_id":"p1","protocol":"uniswap_v2","split":0.0}]}"#;
        let (hops, splits) = compute_route_topology(json);
        assert_eq!(hops, 1);
        assert_eq!(splits, 0);
    }

    #[test]
    fn route_topology_multi_hop_with_split() {
        let json = r#"{"swaps":[
            {"component_id":"p1","split":0.5},
            {"component_id":"p2","split":0.5},
            {"component_id":"p3","split":0.0}
        ]}"#;
        let (hops, splits) = compute_route_topology(json);
        assert_eq!(hops, 3);
        assert_eq!(splits, 2);
    }

    #[test]
    fn route_topology_invalid_json() {
        let (hops, splits) = compute_route_topology("not json");
        assert_eq!(hops, 0);
        assert_eq!(splits, 0);
    }

    #[test]
    fn temporal_features_ethereum_mainnet() {
        // Block 17_001_000 is 1000 blocks after reference (12s each = 12000s)
        let (hour, day) = estimate_temporal_features(17_001_000, 1);
        assert!(hour.is_some());
        assert!(day.is_some());
    }

    #[test]
    fn temporal_features_base_l2() {
        let (hour, day) = estimate_temporal_features(10_001_000, BASE_CHAIN_ID);
        assert!(hour.is_some());
        assert!(day.is_some());
    }

    #[test]
    fn temporal_features_before_reference_returns_none() {
        let (hour, day) = estimate_temporal_features(1_000, 1);
        assert!(hour.is_none());
        assert!(day.is_none());
    }

    #[test]
    fn is_l2_for_base_chain() {
        assert_eq!(BASE_CHAIN_ID, 8453);
    }

    struct TestDirs {
        quote_dir: tempfile::TempDir,
        hop_decay_dir: tempfile::TempDir,
        tycho_route_decay_dir: tempfile::TempDir,
        route_decay_dir: tempfile::TempDir,
    }

    fn make_test_data() -> TestDirs {
        let quote_dir = tempfile::tempdir().expect("create quote dir");
        let hop_decay_dir = tempfile::tempdir().expect("create hop decay dir");
        let tycho_route_decay_dir = tempfile::tempdir().expect("create tycho route dir");
        let route_decay_dir = tempfile::tempdir().expect("create route dir");

        let quote_records = vec![QuoteLogRecord {
            quote_id: "q-1".to_string(),
            solver_id: "solver-a".to_string(),
            request_id: "req-1".to_string(),
            is_winner: true,
            block_number: 17_500_000,
            chain_id: 1,
            amount_in: "1000".to_string(),
            amount_out: "990".to_string(),
            gas_estimate: 100_000,
            algorithm_type: "most_liquid".to_string(),
            n_alternatives: 2,
            gap_to_second_best_bps: None,
            slippage_tolerance: None,
            token_in: "0x0101010101010101010101010101010101010101".to_string(),
            token_out: "0x0202020202020202020202020202020202020202".to_string(),
            route_json: r#"{"swaps":[{"component_id":"p1","protocol":"uniswap_v2","split":0.0},{"component_id":"p2","protocol":"curve","split":0.0}]}"#.to_string(),
            calldata_hex: "abcd".to_string(),
        }];
        write_quote_log_parquet(&quote_dir.path().join("quotes.parquet"), &quote_records)
            .expect("write quote log");

        let hop_records = vec![
            HopDecayRecord {
                quote_id: "q-1".to_string(),
                solver_id: "solver-a".to_string(),
                block_offset: 1,
                hop_index: 0,
                hop_amount_out: "995".to_string(),
                hop_decay_bps: 5.0,
                depth_at_1pct: None,
                depth_at_5pct: None,
                spot_price: None,
                token_price_in_native: None,
            },
            HopDecayRecord {
                quote_id: "q-1".to_string(),
                solver_id: "solver-a".to_string(),
                block_offset: 1,
                hop_index: 1,
                hop_amount_out: "985".to_string(),
                hop_decay_bps: 10.0,
                depth_at_1pct: None,
                depth_at_5pct: None,
                spot_price: None,
                token_price_in_native: None,
            },
        ];
        write_hop_decay_parquet(
            &hop_decay_dir.path().join("hops.parquet"),
            &hop_records,
        )
        .expect("write hop decay");

        let tycho_records = vec![TychoRouteDecayRecord {
            quote_id: "q-1".to_string(),
            solver_id: "solver-a".to_string(),
            block_offset: 1,
            route_total_amount_out: "985".to_string(),
            route_decay_bps: 15.0,
            requote_amount_out: None,
            market_movement_bps: Some(5.0),
            execution_slippage_bps: Some(10.0),
            cex_mid_price: Some(2005.0),
            cex_dex_spread_bps: Some(25.0),
            realized_vol_5m_bps: Some(12.5),
            realized_vol_15m_bps: Some(18.3),
        }];
        write_tycho_route_decay_parquet(
            &tycho_route_decay_dir.path().join("tycho.parquet"),
            &tycho_records,
        )
        .expect("write tycho route decay");

        let route_records = vec![RouteDecayRecord {
            quote_id: "q-1".to_string(),
            solver_id: "solver-a".to_string(),
            request_id: "req-1".to_string(),
            block_offset: 1,
            eth_call_amount_out: "984".to_string(),
            eth_call_gas_used: 150_000,
            eth_call_success: true,
            eth_call_revert_reason: None,
            eth_call_decay_bps: 16.0,
        }];
        write_route_decay_parquet(
            &route_decay_dir.path().join("routes.parquet"),
            &route_records,
        )
        .expect("write route decay");

        TestDirs {
            quote_dir,
            hop_decay_dir,
            tycho_route_decay_dir,
            route_decay_dir,
        }
    }

    #[test]
    fn join_datasets_produces_unified_records() {
        let td = make_test_data();

        let quotes = parse_quote_log_rows(
            &read_parquet_files_from_dir(td.quote_dir.path()).expect("read quotes"),
        );
        let hops = parse_hop_decay_rows(
            &read_parquet_files_from_dir(td.hop_decay_dir.path()).expect("read hops"),
        );
        let tycho = parse_tycho_route_decay_rows(
            &read_parquet_files_from_dir(td.tycho_route_decay_dir.path()).expect("read tycho"),
        );
        let routes = parse_route_decay_rows(
            &read_parquet_files_from_dir(td.route_decay_dir.path()).expect("read routes"),
        );

        let empty_pairs = HashMap::new();
        let empty_cats = HashMap::new();
        let unified =
            join_datasets(&quotes, &hops, &tycho, &routes, &empty_pairs, &empty_cats);

        assert_eq!(unified.len(), 1);

        let r = &unified[0];
        assert_eq!(r.quote_id, "q-1");
        assert_eq!(r.solver_id, "solver-a");
        assert_eq!(r.block_offset, 1);

        assert!((r.max_hop_decay_bps - 10.0).abs() < f64::EPSILON);
        assert!((r.route_decay_bps - 15.0).abs() < f64::EPSILON);
        assert!((r.market_movement_bps.expect("has mm") - 5.0).abs() < f64::EPSILON);
        assert!((r.execution_slippage_bps.expect("has es") - 10.0).abs() < f64::EPSILON);

        assert!(r.is_winner);
        assert_eq!(r.n_alternatives, 2);
        assert!(r.gap_to_second_best_bps.is_none());

        assert_eq!(r.eth_call_amount_out.as_deref(), Some("984"));
        assert_eq!(r.eth_call_gas_used, Some(150_000));
        assert_eq!(r.eth_call_success, Some(true));
        assert!((r.eth_call_decay_bps.expect("has decay") - 16.0).abs() < f64::EPSILON);

        assert_eq!(r.hop_count, 2);
        assert_eq!(r.split_count, 0);
        assert!(!r.is_l2);
        assert_eq!(r.chain_id, 1);
        assert!(r.hour_of_day.is_some());
        assert!(r.day_of_week.is_some());
        assert_eq!(r.token_in_category, "unknown");
        assert_eq!(r.token_out_category, "unknown");
        assert_eq!(r.pair_bucket, "unknown");
    }

    #[test]
    fn join_without_route_decay_still_works() {
        let td = make_test_data();

        let empty_route_dir = tempfile::tempdir().expect("create empty route dir");

        let quotes = parse_quote_log_rows(
            &read_parquet_files_from_dir(td.quote_dir.path()).expect("read quotes"),
        );
        let hops = parse_hop_decay_rows(
            &read_parquet_files_from_dir(td.hop_decay_dir.path()).expect("read hops"),
        );
        let tycho = parse_tycho_route_decay_rows(
            &read_parquet_files_from_dir(td.tycho_route_decay_dir.path()).expect("read tycho"),
        );
        let routes = parse_route_decay_rows(
            &read_parquet_files_from_dir(empty_route_dir.path()).expect("read routes"),
        );

        let empty_pairs = HashMap::new();
        let empty_cats = HashMap::new();
        let unified =
            join_datasets(&quotes, &hops, &tycho, &routes, &empty_pairs, &empty_cats);
        assert_eq!(unified.len(), 1);

        let r = &unified[0];
        assert!(r.eth_call_amount_out.is_none());
        assert!(r.eth_call_gas_used.is_none());
        assert!(r.eth_call_success.is_none());
        assert!(r.eth_call_decay_bps.is_none());
    }

    #[test]
    fn unified_parquet_round_trip() {
        use arrow::array::AsArray;

        let td = make_test_data();
        let output_dir = tempfile::tempdir().expect("create output dir");

        let quotes = parse_quote_log_rows(
            &read_parquet_files_from_dir(td.quote_dir.path()).expect("read quotes"),
        );
        let hops = parse_hop_decay_rows(
            &read_parquet_files_from_dir(td.hop_decay_dir.path()).expect("read hops"),
        );
        let tycho = parse_tycho_route_decay_rows(
            &read_parquet_files_from_dir(td.tycho_route_decay_dir.path()).expect("read tycho"),
        );
        let routes = parse_route_decay_rows(
            &read_parquet_files_from_dir(td.route_decay_dir.path()).expect("read routes"),
        );

        let empty_pairs = HashMap::new();
        let empty_cats = HashMap::new();
        let unified =
            join_datasets(&quotes, &hops, &tycho, &routes, &empty_pairs, &empty_cats);
        write_unified_parquet(output_dir.path(), &unified).expect("write unified parquet");

        let chain_dir = output_dir.path().join("chain_id=1");
        assert!(chain_dir.exists());

        let parquet_path = chain_dir.join("unified.parquet");
        let file = std::fs::File::open(&parquet_path).expect("open parquet");
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("build reader");
        let mut reader = builder.build().expect("build reader");

        let batch = reader
            .next()
            .expect("has batch")
            .expect("batch ok");
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 34);

        let quote_ids = batch
            .column_by_name("quote_id")
            .and_then(|c| c.as_string_opt::<i32>())
            .expect("quote_id column");
        assert_eq!(quote_ids.value(0), "q-1");

        let hop_counts = batch
            .column_by_name("hop_count")
            .and_then(|c| c.as_primitive_opt::<arrow::datatypes::UInt32Type>())
            .expect("hop_count column");
        assert_eq!(hop_counts.value(0), 2);

        let pair_buckets = batch
            .column_by_name("pair_bucket")
            .and_then(|c| c.as_string_opt::<i32>())
            .expect("pair_bucket column");
        assert_eq!(pair_buckets.value(0), "unknown");
    }

    #[test]
    fn empty_sources_produce_no_output() {
        let empty1 = tempfile::tempdir().expect("dir");
        let empty2 = tempfile::tempdir().expect("dir");
        let empty3 = tempfile::tempdir().expect("dir");
        let empty4 = tempfile::tempdir().expect("dir");

        let quotes = parse_quote_log_rows(
            &read_parquet_files_from_dir(empty1.path()).expect("read"),
        );
        let hops = parse_hop_decay_rows(
            &read_parquet_files_from_dir(empty2.path()).expect("read"),
        );
        let tycho = parse_tycho_route_decay_rows(
            &read_parquet_files_from_dir(empty3.path()).expect("read"),
        );
        let routes = parse_route_decay_rows(
            &read_parquet_files_from_dir(empty4.path()).expect("read"),
        );

        let empty_pairs = HashMap::new();
        let empty_cats = HashMap::new();
        let unified = join_datasets(&quotes, &hops, &tycho, &routes, &empty_pairs, &empty_cats);
        assert!(unified.is_empty());
    }

    #[test]
    fn unified_schema_has_expected_field_count() {
        let schema = unified_schema();
        assert_eq!(schema.fields().len(), 34);
    }

    #[test]
    fn gap_detection_no_gaps() {
        let rows: Vec<QuoteLogRow> = (0..5)
            .map(|i| QuoteLogRow {
                quote_id: format!("q-{i}"),
                solver_id: "solver-a".to_string(),
                request_id: "req-1".to_string(),
                is_winner: true,
                block_number: 17_500_000 + i * 10,
                chain_id: 1,
                amount_in: "1000".to_string(),
                amount_out: "990".to_string(),
                gas_estimate: 100_000,
                algorithm_type: "most_liquid".to_string(),
                n_alternatives: 1,
                gap_to_second_best_bps: None,
                token_in: "0x01".to_string(),
                token_out: "0x02".to_string(),
                route_json: "{}".to_string(),
            })
            .collect();
        // Should not panic; gaps below threshold of 100.
        detect_block_gaps(&rows, 100);
    }

    #[test]
    fn gap_detection_with_gap() {
        let rows = vec![
            QuoteLogRow {
                quote_id: "q-0".to_string(),
                solver_id: "solver-a".to_string(),
                request_id: "req-1".to_string(),
                is_winner: true,
                block_number: 17_500_000,
                chain_id: 1,
                amount_in: "1000".to_string(),
                amount_out: "990".to_string(),
                gas_estimate: 100_000,
                algorithm_type: "most_liquid".to_string(),
                n_alternatives: 1,
                gap_to_second_best_bps: None,
                token_in: "0x01".to_string(),
                token_out: "0x02".to_string(),
                route_json: "{}".to_string(),
            },
            QuoteLogRow {
                quote_id: "q-1".to_string(),
                solver_id: "solver-a".to_string(),
                request_id: "req-1".to_string(),
                is_winner: true,
                block_number: 17_500_500,
                chain_id: 1,
                amount_in: "1000".to_string(),
                amount_out: "990".to_string(),
                gas_estimate: 100_000,
                algorithm_type: "most_liquid".to_string(),
                n_alternatives: 1,
                gap_to_second_best_bps: None,
                token_in: "0x01".to_string(),
                token_out: "0x02".to_string(),
                route_json: "{}".to_string(),
            },
        ];
        // Gap of 500 blocks > threshold of 100: should detect it.
        detect_block_gaps(&rows, 100);
    }

    #[test]
    fn gap_detection_empty_input() {
        detect_block_gaps(&[], 100);
    }

    #[test]
    fn gap_detection_single_block() {
        let rows = vec![QuoteLogRow {
            quote_id: "q-0".to_string(),
            solver_id: "solver-a".to_string(),
            request_id: "req-1".to_string(),
            is_winner: true,
            block_number: 17_500_000,
            chain_id: 1,
            amount_in: "1000".to_string(),
            amount_out: "990".to_string(),
            gas_estimate: 100_000,
            algorithm_type: "most_liquid".to_string(),
            n_alternatives: 1,
            gap_to_second_best_bps: None,
            token_in: "0x01".to_string(),
            token_out: "0x02".to_string(),
            route_json: "{}".to_string(),
        }];
        detect_block_gaps(&rows, 100);
    }

    #[test]
    fn cli_gap_threshold_custom() {
        let args = Args::parse_from([
            "assemble",
            "--quote-log-dir",
            "/tmp/quotes",
            "--hop-decay-dir",
            "/tmp/hops",
            "--tycho-route-decay-dir",
            "/tmp/tycho",
            "--route-decay-dir",
            "/tmp/routes",
            "--output-dir",
            "/tmp/out",
            "--gap-threshold",
            "50",
        ]);
        assert_eq!(args.gap_threshold, 50);
    }
}
