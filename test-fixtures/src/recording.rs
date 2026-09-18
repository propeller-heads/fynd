//! Market recording types and zstd-compressed I/O.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tycho_simulation::protocol::models::Update;

/// Metadata about a recording session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingMetadata {
    /// Chain name (e.g. `"ethereum"`).
    pub chain: String,
    /// Unix timestamp (seconds) when recording started.
    pub recorded_at_secs: u64,
    /// Fynd crate version at recording time.
    pub fynd_version: String,
    /// Actual recording wall-clock duration in seconds.
    pub recording_duration_secs: u64,
    /// Protocol systems included in the recording.
    pub protocols: Vec<String>,
    /// Minimum TVL filter used during recording.
    pub min_tvl: f64,
    /// Minimum token quality filter used during recording.
    pub min_token_quality: i32,
    /// Token recency filter (days).
    pub traded_n_days_ago: Option<u64>,
    /// Gas price in wei captured from RPC at recording time.
    /// Stored as a decimal string to preserve full precision.
    #[serde(default)]
    pub gas_price_wei: Option<String>,
    /// SHA-256 hash of worker_pools.toml at expected output generation time.
    /// Integration tests warn if the current file's hash differs.
    #[serde(default)]
    pub worker_pools_hash: Option<String>,
    /// Recording format version for forward compatibility.
    #[serde(default)]
    pub schema_version: u32,
}

impl RecordingMetadata {
    /// Parse the stored gas price wei string into a `BigUint`.
    /// Returns `None` if not recorded.
    pub fn gas_price_as_biguint(&self) -> Option<num_bigint::BigUint> {
        self.gas_price_wei
            .as_deref()
            .and_then(|s| match s.parse() {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        gas_price_wei = s,
                        error = %e,
                        "failed to parse recorded gas price"
                    );
                    None
                }
            })
    }
}

/// A complete market recording: metadata + ordered `Update` messages.
///
/// `Update` is serialized directly (tycho-simulation >= 0.256). VM-backed
/// protocol states that can't be serialized are silently skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRecording {
    /// Recording session metadata.
    pub metadata: RecordingMetadata,
    /// Ordered sequence of stream updates to replay.
    pub updates: Vec<Update>,
}

/// Write a [`MarketRecording`] to a zstd-compressed JSON file.
pub fn write_recording(recording: &MarketRecording, path: &Path) -> anyhow::Result<()> {
    let json = serde_json::to_vec(recording)?;
    let compressed = zstd::encode_all(json.as_slice(), 3)?;
    std::fs::write(path, compressed)?;
    Ok(())
}

/// Read a [`MarketRecording`] from a zstd-compressed JSON file.
pub fn read_recording(path: &Path) -> anyhow::Result<MarketRecording> {
    let compressed = std::fs::read(path)?;
    let decompressed = zstd::decode_all(compressed.as_slice())?;
    let recording: MarketRecording = serde_json::from_slice(&decompressed)?;
    Ok(recording)
}

/// Compute SHA-256 hex digest of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn write_read_roundtrip_empty() {
        let recording = MarketRecording {
            metadata: RecordingMetadata {
                chain: "ethereum".to_string(),
                recorded_at_secs: 1710000000,
                fynd_version: "0.46.0".to_string(),
                recording_duration_secs: 30,
                protocols: vec!["uniswap_v2".to_string()],
                min_tvl: 10.0,
                min_token_quality: 100,
                traded_n_days_ago: Some(3),
                gas_price_wei: Some("15000000000".to_string()),
                worker_pools_hash: None,
                schema_version: 1,
            },
            updates: vec![Update::new(12345, HashMap::new(), HashMap::new())],
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json.zst");

        write_recording(&recording, &path).unwrap();
        let loaded = read_recording(&path).unwrap();

        assert_eq!(loaded.metadata.chain, "ethereum");
        assert_eq!(loaded.updates.len(), 1);
        assert_eq!(loaded.updates[0].block_number, 12345);
    }
}
