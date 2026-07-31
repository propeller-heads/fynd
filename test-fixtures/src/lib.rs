//! Shared types for Fynd integration test recordings and expected outputs.
//!
//! Used by:
//! - `record-market` tool (writes recordings + expected files)
//! - `fynd-core` integration tests (reads and verifies)

pub mod expected;
pub mod recording;
pub mod scenarios;

pub use expected::{
    DerivedDataMetrics, ExpectedFile, ExpectedMetadata, ExpectedOutput, ExpectedScenario,
};
pub use recording::{read_recording, write_recording, MarketRecording, RecordingMetadata};
pub use scenarios::{load_test_scenarios, TestScenario};

/// Parse a `worker_pools.toml` string into a pool name → config map.
pub fn parse_pools_toml(
    toml_content: &str,
) -> anyhow::Result<std::collections::HashMap<String, fynd_core::PoolConfig>> {
    #[derive(serde::Deserialize)]
    struct PoolsFile {
        pools: std::collections::HashMap<String, fynd_core::PoolConfig>,
    }
    let config: PoolsFile = toml::from_str(toml_content)?;
    Ok(config.pools)
}
