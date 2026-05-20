//! Ground-truth resimulation binary: reads quote log parquet files and
//! resimulates routes via `eth_call` with storage overrides against an
//! archive node.

use std::path::PathBuf;

use alloy::{
    eips::BlockId,
    primitives::{keccak256, map::B256HashMap, Address, Bytes, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionRequest,
    },
};
use arrow::array::AsArray;
use clap::Parser;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use slippage_features::{
    decay::compute_decay_bps,
    parquet_writer::{write_route_decay_parquet, RouteDecayRecord},
};
use tracing::{debug, info, warn};

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

    /// Router contract address
    #[arg(long, default_value = "0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35")]
    router_address: String,

    /// Sender address for eth_call
    #[arg(long, default_value = "0x0000000000000000000000000000000000000001")]
    sender: String,

    /// Balance storage slot position (default: 0, common for most ERC-20s)
    #[arg(long, default_value_t = 0)]
    balance_slot_position: u64,

    /// Allowance storage slot position (default: 1, common for most ERC-20s)
    #[arg(long, default_value_t = 1)]
    allowance_slot_position: u64,
}

/// A parsed quote log row with the fields needed for resimulation.
struct QuoteRecord {
    quote_id: String,
    solver_id: String,
    request_id: String,
    block_number: u64,
    amount_out: String,
    calldata_hex: String,
    route_json: String,
}

// --- Storage slot helpers (copied from tools/fynd-swap-cli/src/erc20.rs) ---

/// Compute the keccak256 storage slot for `mapping(address => uint256)` at
/// a given Solidity mapping position (e.g. `balanceOf` storage).
fn balance_slot_at(holder: Address, position: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[56..64].copy_from_slice(&position.to_be_bytes());
    keccak256(buf)
}

/// Compute the keccak256 storage slot for a nested mapping
/// `mapping(address => mapping(address => uint256))` (e.g. `allowance`).
fn allowance_slot_at(owner: Address, spender: Address, position: u64) -> B256 {
    let inner = balance_slot_at(owner, position);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(spender.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

/// Extract the first hop's `token_in` address from the route JSON.
///
/// The route JSON has the shape `{"swaps": [{"token_in": "0x...", ...}, ...]}`.
/// Returns `None` if the JSON is malformed or has no swaps.
fn parse_token_in_from_route(route_json: &str) -> Option<Address> {
    let parsed: serde_json::Value = serde_json::from_str(route_json).ok()?;
    let token_in_str = parsed["swaps"].as_array()?.first()?["token_in"].as_str()?;
    token_in_str.parse::<Address>().ok()
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
            let route_jsons = batch
                .column_by_name("route_json")
                .and_then(|c| c.as_string_opt::<i32>());

            let (
                Some(quote_ids),
                Some(solver_ids),
                Some(request_ids),
                Some(block_numbers),
                Some(amounts_out),
                Some(calldatas),
                Some(route_jsons),
            ) = (
                quote_ids,
                solver_ids,
                request_ids,
                block_numbers,
                amounts_out,
                calldatas,
                route_jsons,
            )
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
                    route_json: route_jsons.value(row).to_string(),
                });
            }
        }
    }

    info!(count = records.len(), "loaded quote records");
    Ok(records)
}

/// Build ERC-20 balance+allowance state overrides so the sender appears to
/// hold (and have approved) an effectively unlimited token balance.
fn build_token_overrides(
    token_in: Address,
    sender: Address,
    router: Address,
    balance_position: u64,
    allowance_position: u64,
) -> StateOverride {
    let sentinel = B256::from(U256::MAX >> 1);
    let bal_slot = balance_slot_at(sender, balance_position);
    let allow_slot = allowance_slot_at(sender, router, allowance_position);

    let mut state_diff = B256HashMap::default();
    state_diff.insert(bal_slot, sentinel);
    state_diff.insert(allow_slot, sentinel);

    let mut overrides = StateOverride::default();
    overrides
        .insert(token_in, AccountOverride { state_diff: Some(state_diff), ..Default::default() });
    overrides
}

/// Resimulate a single quote at a given block offset via eth_call.
///
/// Returns a `RouteDecayRecord` for the (quote, block_offset) pair.
async fn resim_quote_at_offset(
    provider: &impl Provider,
    record: &QuoteRecord,
    block_offset: u32,
    sender: Address,
    router: Address,
    overrides: &StateOverride,
) -> RouteDecayRecord {
    let target_block = BlockId::number(record.block_number + u64::from(block_offset));

    let calldata_bytes = match hex::decode(&record.calldata_hex) {
        Ok(b) => b,
        Err(e) => {
            return RouteDecayRecord {
                quote_id: record.quote_id.clone(),
                solver_id: record.solver_id.clone(),
                request_id: record.request_id.clone(),
                block_offset,
                eth_call_amount_out: "0".to_string(),
                eth_call_gas_used: 0,
                eth_call_success: false,
                eth_call_revert_reason: Some(format!("hex decode error: {e}")),
                eth_call_decay_bps: f64::NAN,
            };
        }
    };

    let req = TransactionRequest {
        from: Some(sender),
        to: Some(alloy::primitives::TxKind::Call(router)),
        input: Bytes::from(calldata_bytes).into(),
        gas_price: Some(0),
        ..Default::default()
    };

    let return_data = provider
        .call(req.clone())
        .block(target_block)
        .overrides(overrides.clone())
        .await;

    let gas_used = provider
        .estimate_gas(req)
        .block(target_block)
        .overrides(overrides.clone())
        .await
        .unwrap_or(0);

    match return_data {
        Ok(data) if data.len() >= 32 => {
            let amount_out = num_bigint::BigUint::from_bytes_be(&data[0..32]);
            let amount_out_str = amount_out.to_string();
            let decay_bps =
                compute_decay_bps(&record.amount_out, &amount_out_str).unwrap_or(f64::NAN);

            debug!(
                quote_id = %record.quote_id,
                block_offset,
                amount_out = %amount_out_str,
                decay_bps,
                gas_used,
                "eth_call succeeded"
            );

            RouteDecayRecord {
                quote_id: record.quote_id.clone(),
                solver_id: record.solver_id.clone(),
                request_id: record.request_id.clone(),
                block_offset,
                eth_call_amount_out: amount_out_str,
                eth_call_gas_used: gas_used,
                eth_call_success: true,
                eth_call_revert_reason: None,
                eth_call_decay_bps: decay_bps,
            }
        }
        Ok(_) => {
            warn!(
                quote_id = %record.quote_id,
                block_offset,
                "eth_call returned empty data (treating as revert)"
            );
            RouteDecayRecord {
                quote_id: record.quote_id.clone(),
                solver_id: record.solver_id.clone(),
                request_id: record.request_id.clone(),
                block_offset,
                eth_call_amount_out: "0".to_string(),
                eth_call_gas_used: gas_used,
                eth_call_success: false,
                eth_call_revert_reason: Some("empty return data".to_string()),
                eth_call_decay_bps: f64::NAN,
            }
        }
        Err(e) => {
            let reason = format!("{e}");
            debug!(
                quote_id = %record.quote_id,
                block_offset,
                reason = %reason,
                "eth_call reverted"
            );
            RouteDecayRecord {
                quote_id: record.quote_id.clone(),
                solver_id: record.solver_id.clone(),
                request_id: record.request_id.clone(),
                block_offset,
                eth_call_amount_out: "0".to_string(),
                eth_call_gas_used: gas_used,
                eth_call_success: false,
                eth_call_revert_reason: Some(reason),
                eth_call_decay_bps: f64::NAN,
            }
        }
    }
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

    let sender: Address = args
        .sender
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid sender address '{}': {e}", args.sender))?;
    let router: Address = args
        .router_address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid router address '{}': {e}", args.router_address))?;

    info!(
        quote_log_dir = %args.quote_log_dir.display(),
        output_dir = %args.output_dir.display(),
        rpc_url = %args.rpc_url,
        max_block_offset = args.max_block_offset,
        sender = %sender,
        router = %router,
        balance_slot_position = args.balance_slot_position,
        allowance_slot_position = args.allowance_slot_position,
        "starting node resim"
    );

    std::fs::create_dir_all(&args.output_dir)?;

    let quotes = read_quote_log_files(&args.quote_log_dir)?;
    if quotes.is_empty() {
        info!("no quotes to resimulate");
        return Ok(());
    }

    let provider = ProviderBuilder::new().connect_http(args.rpc_url.parse()?);

    let mut all_records: Vec<RouteDecayRecord> = Vec::new();
    let mut skipped = 0u64;

    for (idx, quote) in quotes.iter().enumerate() {
        if quote.calldata_hex.is_empty() {
            debug!(quote_id = %quote.quote_id, "skipping quote with empty calldata");
            skipped += 1;
            continue;
        }

        let token_in = match parse_token_in_from_route(&quote.route_json) {
            Some(addr) => addr,
            None => {
                warn!(
                    quote_id = %quote.quote_id,
                    route_json = %quote.route_json,
                    "cannot parse token_in from route JSON, skipping"
                );
                skipped += 1;
                continue;
            }
        };

        let overrides = build_token_overrides(
            token_in,
            sender,
            router,
            args.balance_slot_position,
            args.allowance_slot_position,
        );

        for offset in 1..=args.max_block_offset {
            let record =
                resim_quote_at_offset(&provider, quote, offset, sender, router, &overrides).await;
            all_records.push(record);
        }

        if (idx + 1) % 100 == 0 {
            info!(progress = idx + 1, total = quotes.len(), "resimulation progress");
        }
    }

    info!(resimulated = quotes.len() - skipped as usize, skipped, "finished resimulation");

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
        assert_eq!(args.router_address, "0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35");
        assert_eq!(args.sender, "0x0000000000000000000000000000000000000001");
        assert_eq!(args.balance_slot_position, 0);
        assert_eq!(args.allowance_slot_position, 1);
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
    fn cli_parsing_custom_addresses() {
        let args = Args::parse_from([
            "node-resim",
            "--quote-log-dir",
            "/tmp/logs",
            "--output-dir",
            "/tmp/out",
            "--rpc-url",
            "http://localhost:8545",
            "--router-address",
            "0xDEAD000000000000000000000000000000000000",
            "--sender",
            "0xBEEF000000000000000000000000000000000000",
            "--balance-slot-position",
            "3",
            "--allowance-slot-position",
            "4",
        ]);
        assert_eq!(args.router_address, "0xDEAD000000000000000000000000000000000000");
        assert_eq!(args.sender, "0xBEEF000000000000000000000000000000000000");
        assert_eq!(args.balance_slot_position, 3);
        assert_eq!(args.allowance_slot_position, 4);
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

        let route_json = r#"{"swaps":[{"token_in":"0x0101010101010101010101010101010101010101","token_out":"0x0202020202020202020202020202020202020202"}]}"#;

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
                route_json: route_json.to_string(),
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
                route_json: route_json.to_string(),
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
        assert_eq!(records[0].route_json, route_json);

        assert_eq!(records[1].quote_id, "q-2");
        assert_eq!(records[1].block_number, 101);
        assert_eq!(records[1].calldata_hex, "deadbeef");
    }

    #[test]
    fn parse_token_in_valid_route() {
        let json = r#"{"swaps":[{"token_in":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","token_out":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"}]}"#;
        let addr = parse_token_in_from_route(json).unwrap();
        let expected: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        assert_eq!(addr, expected);
    }

    #[test]
    fn parse_token_in_empty_swaps() {
        let json = r#"{"swaps":[]}"#;
        assert!(parse_token_in_from_route(json).is_none());
    }

    #[test]
    fn parse_token_in_invalid_json() {
        assert!(parse_token_in_from_route("not json").is_none());
    }

    #[test]
    fn parse_token_in_missing_field() {
        let json = r#"{"swaps":[{"token_out":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"}]}"#;
        assert!(parse_token_in_from_route(json).is_none());
    }

    #[test]
    fn balance_slot_matches_erc20_helper() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let slot0 = balance_slot_at(addr, 0);
        let slot1 = balance_slot_at(addr, 1);
        assert_ne!(slot0, slot1);
    }

    #[test]
    fn allowance_slot_matches_erc20_helper() {
        let owner: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let spender: Address = "0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35"
            .parse()
            .unwrap();
        let slot = allowance_slot_at(owner, spender, 1);
        assert_ne!(slot, B256::ZERO);
    }

    #[test]
    fn build_token_overrides_contains_two_slots() {
        let token: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let sender: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let router: Address = "0xfD0b31d2E955fA55e3fa641Fe90e08b677188d35"
            .parse()
            .unwrap();

        let overrides = build_token_overrides(token, sender, router, 0, 1);
        let account = overrides
            .get(&token)
            .expect("token override missing");
        let diff = account
            .state_diff
            .as_ref()
            .expect("state_diff missing");
        assert_eq!(diff.len(), 2, "should have balance + allowance slots");
    }
}
