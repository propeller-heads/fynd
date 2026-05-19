//! Ground-truth resimulation binary: reads quote log parquet files and
//! resimulates routes via `eth_call` with storage overrides against an
//! archive node.

use std::path::PathBuf;

use arrow::array::AsArray;
use clap::Parser;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use slippage_features::parquet_writer::{write_route_decay_parquet, RouteDecayRecord};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "node-resim", about = "Ground-truth resimulation via eth_call")]
struct Args {
    /// Path to quote log parquet directory
    #[arg(long)]
    quote_log_dir: PathBuf,

    /// Output directory for route-level decay parquet
    #[arg(long)]
    output_dir: PathBuf,

    /// RPC URL (archive node)
    #[arg(long, env = "ETH_RPC_URL")]
    rpc_url: String,

    /// Max block offset (default: 10)
    #[arg(long, default_value_t = 10)]
    max_block_offset: u32,
}

/// A parsed quote log row with the fields needed for resimulation.
///
/// All fields are populated by `read_quote_log_files` and consumed by
/// `resim_quote_at_offset` once eth_call integration is wired up.
#[allow(dead_code)]
struct QuoteRecord {
    quote_id: String,
    solver_id: String,
    request_id: String,
    block_number: u64,
    amount_out: String,
    calldata_hex: String,
}

fn read_quote_log_files(dir: &PathBuf) -> anyhow::Result<Vec<QuoteRecord>> {
    let mut records = Vec::new();

    let entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "parquet")
        })
        .collect();

    if entries.is_empty() {
        warn!(dir = %dir.display(), "no parquet files found in quote log directory");
        return Ok(records);
    }

    for entry in &entries {
        let path = entry.path();
        info!(path = %path.display(), "reading quote log parquet");

        let file = std::fs::File::open(&path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let reader = builder.build()?;

        for batch_result in reader {
            let batch = batch_result?;

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
            let amounts_out = batch
                .column_by_name("amount_out")
                .and_then(|c| c.as_string_opt::<i32>());
            let calldatas = batch
                .column_by_name("calldata_hex")
                .and_then(|c| c.as_string_opt::<i32>());

            let (
                Some(quote_ids),
                Some(solver_ids),
                Some(request_ids),
                Some(block_numbers),
                Some(amounts_out),
                Some(calldatas),
            ) = (quote_ids, solver_ids, request_ids, block_numbers, amounts_out, calldatas)
            else {
                warn!(
                    path = %path.display(),
                    "missing expected columns in quote log parquet, skipping file"
                );
                continue;
            };

            for row in 0..batch.num_rows() {
                records.push(QuoteRecord {
                    quote_id: quote_ids.value(row).to_string(),
                    solver_id: solver_ids.value(row).to_string(),
                    request_id: request_ids.value(row).to_string(),
                    block_number: block_numbers.value(row),
                    amount_out: amounts_out.value(row).to_string(),
                    calldata_hex: calldatas.value(row).to_string(),
                });
            }
        }
    }

    info!(count = records.len(), "loaded quote records");
    Ok(records)
}

/// Resimulate a single quote at a given block offset via eth_call.
///
/// Returns a `RouteDecayRecord` for the (quote, block_offset) pair.
async fn resim_quote_at_offset(
    _rpc_url: &str,
    record: &QuoteRecord,
    block_offset: u32,
) -> RouteDecayRecord {
    // The actual eth_call integration requires:
    //  1. Decoding the calldata to identify the router contract and call params
    //  2. Setting slippage tolerance to 100% in the calldata
    //  3. Building a provider with the archive block parameter (block + offset)
    //  4. Constructing StateOverride for balance/allowance slots
    //  5. Calling eth_call and decoding amount_out from the return bytes
    //
    // See tools/fynd-swap-cli/src/erc20.rs for the state_override_single()
    // pattern and the eth_call invocation.
    let _ = &record.calldata_hex;
    let _target_block = record.block_number + u64::from(block_offset);

    todo!(
        "Wire up eth_call: decode calldata, set 100% slippage, \
         call at block {} with storage overrides",
        _target_block
    );
}

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
        output_dir = %args.output_dir.display(),
        rpc_url = %args.rpc_url,
        max_block_offset = args.max_block_offset,
        "starting node resim"
    );

    std::fs::create_dir_all(&args.output_dir)?;

    let quotes = read_quote_log_files(&args.quote_log_dir)?;
    if quotes.is_empty() {
        info!("no quotes to resimulate");
        return Ok(());
    }

    let mut all_records: Vec<RouteDecayRecord> = Vec::new();

    for quote in &quotes {
        for offset in 1..=args.max_block_offset {
            let record = resim_quote_at_offset(&args.rpc_url, quote, offset).await;
            all_records.push(record);
        }
    }

    let output_path = args
        .output_dir
        .join("route_decay.parquet");
    write_route_decay_parquet(&output_path, &all_records)?;

    info!(
        records = all_records.len(),
        path = %output_path.display(),
        "wrote route decay parquet"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parsing_defaults() {
        let args = Args::parse_from([
            "node-resim",
            "--quote-log-dir",
            "/tmp/logs",
            "--output-dir",
            "/tmp/out",
            "--rpc-url",
            "http://localhost:8545",
        ]);
        assert_eq!(args.quote_log_dir, PathBuf::from("/tmp/logs"));
        assert_eq!(args.output_dir, PathBuf::from("/tmp/out"));
        assert_eq!(args.rpc_url, "http://localhost:8545");
        assert_eq!(args.max_block_offset, 10);
    }

    #[test]
    fn cli_parsing_custom_offset() {
        let args = Args::parse_from([
            "node-resim",
            "--quote-log-dir",
            "/tmp/logs",
            "--output-dir",
            "/tmp/out",
            "--rpc-url",
            "http://localhost:8545",
            "--max-block-offset",
            "5",
        ]);
        assert_eq!(args.max_block_offset, 5);
    }

    #[test]
    fn read_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let records = read_quote_log_files(&dir.path().to_path_buf()).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn read_quote_log_parquet_round_trip() {
        use slippage_features::parquet_writer::{write_quote_log_parquet, QuoteLogRecord};

        let dir = tempfile::tempdir().unwrap();
        let parquet_path = dir.path().join("test_quotes.parquet");

        let log_records = vec![
            QuoteLogRecord {
                quote_id: "q-1".to_string(),
                solver_id: "solver-a".to_string(),
                request_id: "req-1".to_string(),
                is_winner: true,
                block_number: 100,
                chain_id: 1,
                amount_in: "1000".to_string(),
                amount_out: "990".to_string(),
                gas_estimate: 50_000,
                algorithm_type: "most_liquid".to_string(),
                n_alternatives: 2,
                gap_to_second_best_bps: None,
                slippage_tolerance: None,
                route_json: "{}".to_string(),
                calldata_hex: "abcd1234".to_string(),
            },
            QuoteLogRecord {
                quote_id: "q-2".to_string(),
                solver_id: "solver-b".to_string(),
                request_id: "req-2".to_string(),
                is_winner: false,
                block_number: 101,
                chain_id: 1,
                amount_in: "2000".to_string(),
                amount_out: "1980".to_string(),
                gas_estimate: 75_000,
                algorithm_type: "bellman_ford".to_string(),
                n_alternatives: 1,
                gap_to_second_best_bps: Some(5.0),
                slippage_tolerance: Some(0.01),
                route_json: "{}".to_string(),
                calldata_hex: "deadbeef".to_string(),
            },
        ];

        write_quote_log_parquet(&parquet_path, &log_records).unwrap();

        let records = read_quote_log_files(&dir.path().to_path_buf()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].quote_id, "q-1");
        assert_eq!(records[0].solver_id, "solver-a");
        assert_eq!(records[0].block_number, 100);
        assert_eq!(records[0].amount_out, "990");
        assert_eq!(records[0].calldata_hex, "abcd1234");

        assert_eq!(records[1].quote_id, "q-2");
        assert_eq!(records[1].block_number, 101);
        assert_eq!(records[1].calldata_hex, "deadbeef");
    }
}
