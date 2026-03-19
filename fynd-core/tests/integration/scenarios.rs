
use std::path::PathBuf;

use fynd_core::types::{Order, OrderSide, QuoteStatus};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestScenario {
    pub token_in: Address,
    pub token_out: Address,
    pub amount: BigUint,
    pub side: OrderSide,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenOutput {
    pub status: QuoteStatus,
    pub amount_out_net_gas: BigUint,
    pub gas_estimate: BigUint,
    pub num_swaps: usize,
    pub solve_time_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenFile {
    pub metadata: GoldenMetadata,
    pub scenarios: Vec<GoldenScenario>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenMetadata {
    pub block_number: u64,
    pub num_pools: usize,
    pub num_tokens: usize,
    pub fynd_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenScenario {
    pub scenario: TestScenario,
    pub expected: GoldenOutput,
}

impl TestScenario {
    pub fn to_order(&self) -> Order {
        Order::new(
            self.token_in.clone(),
            self.token_out.clone(),
            self.amount.clone(),
            self.side,
            Address::zero(20), // dummy sender for testing
        )
    }
}

pub fn golden_file_path() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/integration/golden_outputs.json")
}

/// Load the golden outputs file (expected test results).
/// Returns None if the file doesn't exist (first run before recording).
pub fn load_golden_file() -> Option<GoldenFile> {
    let path = golden_file_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).expect("failed to read golden_outputs.json");
    Some(serde_json::from_str(&content).expect("failed to parse golden_outputs.json"))
}

/// Load test scenarios from pairs.json (the canonical source of trading pairs).
///
/// Scenarios are defined independently of golden outputs so that BLESS_GOLDEN
/// can generate golden_outputs.json from scratch without circular dependency.
///
/// Takes the first amount per pair for a representative subset (~46 scenarios).
pub fn load_test_scenarios() -> Vec<TestScenario> {
    let content = include_str!("../../../tools/benchmark/src/pairs.json");
    let raw: serde_json::Value = serde_json::from_str(content).expect("failed to parse pairs.json");

    // Build token lookup: symbol -> (address, decimals)
    let tokens: std::collections::HashMap<String, (Address, u32)> = raw["tokens"]
        .as_array()
        .expect("tokens should be an array")
        .iter()
        .map(|t| {
            let symbol = t["symbol"].as_str().expect("symbol").to_string();
            let address: Address = t["address"].as_str().expect("address").parse().expect("valid address");
            let decimals = t["decimals"].as_u64().expect("decimals") as u32;
            (symbol, (address, decimals))
        })
        .collect();

    raw["pairs"]
        .as_array()
        .expect("pairs should be an array")
        .iter()
        .map(|pair| {
            let token_in_sym = pair["token_in"].as_str().expect("token_in");
            let token_out_sym = pair["token_out"].as_str().expect("token_out");
            let (token_in, decimals_in) = tokens.get(token_in_sym)
                .unwrap_or_else(|| panic!("unknown token: {token_in_sym}"));
            let (token_out, _) = tokens.get(token_out_sym)
                .unwrap_or_else(|| panic!("unknown token: {token_out_sym}"));

            // Take the first amount (human-readable) and scale by decimals
            let human_amount = pair["amounts"][0].as_f64().expect("amount");
            let raw_amount = human_amount * 10_f64.powi(*decimals_in as i32);
            let amount = BigUint::from(raw_amount as u128);

            TestScenario {
                name: format!("{token_in_sym}_to_{token_out_sym}_{human_amount}"),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                amount,
                side: OrderSide::Sell,
            }
        })
        .collect()
}

pub fn should_bless() -> bool {
    std::env::var("BLESS_GOLDEN").is_ok()
}

pub fn write_golden_file(golden: &GoldenFile) {
    let path = golden_file_path();
    let json =
        serde_json::to_string_pretty(golden).expect("failed to serialize golden file");
    std::fs::write(&path, json).expect("failed to write golden file");
    eprintln!("Golden outputs written to {}", path.display());
}
