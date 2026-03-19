// TODO(ENG-5578): remove when test modules (Tasks 5-8) are implemented and use these types.
#![allow(dead_code)]

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
pub fn load_test_scenarios() -> Vec<TestScenario> {
    todo!("Parse pairs.json into TestScenario vec — exact format TBD during impl")
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
