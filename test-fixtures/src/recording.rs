//! Market recording types and zstd-compressed I/O.
//!
//! A recording holds the raw Tycho feed messages, not decoded states. Replay decodes them with the
//! decoders a live feed uses, so replay keeps every state a live feed produces. That includes the
//! states that cannot be serialized: Uniswap v4 and VM-backed pools, and Uniswap v3 pools with
//! liquidity wider than 64 bits.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tycho_simulation::{
    protocol::models::Update,
    tycho_client::feed::{dto, FeedMessage},
    tycho_common::models::token::Token,
};

/// The recording format this crate writes and reads. Version 1 held decoded `Update`s.
pub const SCHEMA_VERSION: u32 = 2;

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
    /// The resolved `--protocols` entries. The recorder streams these protocols, and replay
    /// registers their decoders.
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
    /// Recording format version, [`SCHEMA_VERSION`] for every file this crate writes.
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

/// A complete market recording: metadata, the token list, and the raw Tycho feed messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRecording {
    /// Recording session metadata.
    pub metadata: RecordingMetadata,
    /// Every token Tycho served under the recording's quality and recency filters. The decoder
    /// needs them to build components; a live feed loads the same list at startup.
    pub tokens: Vec<Token>,
    /// Feed messages in arrival order. The first is the snapshot, the rest are block deltas.
    pub messages: Vec<dto::FeedMessage>,
}

impl MarketRecording {
    /// Decodes the messages into the `Update`s a live feed would have produced from them.
    ///
    /// # Errors
    ///
    /// Fails when the chain is unknown, the recorded protocol list is invalid, or a message does
    /// not decode.
    pub async fn decode_updates(&self) -> anyhow::Result<Vec<Update>> {
        let chain = fynd_core::types::parse_chain(&self.metadata.chain)?;
        let min_token_quality =
            u32::try_from(self.metadata.min_token_quality).with_context(|| {
                format!("min_token_quality {} is negative", self.metadata.min_token_quality)
            })?;
        // Each message is copied and converted just before it is decoded, so the recording is
        // never held twice.
        let messages = self
            .messages
            .iter()
            .cloned()
            .map(FeedMessage::from);
        fynd_core::feed::protocol_registry::decode_recorded_messages(
            chain,
            &self.metadata.protocols,
            min_token_quality,
            self.tokens.iter().cloned(),
            messages,
        )
        .await
        .map_err(anyhow::Error::msg)
    }
}

/// Write a [`MarketRecording`] to a zstd-compressed JSON file.
pub fn write_recording(recording: &MarketRecording, path: &Path) -> anyhow::Result<()> {
    let json = serde_json::to_vec(recording)?;
    let compressed = zstd::encode_all(json.as_slice(), 3)?;
    std::fs::write(path, compressed)?;
    Ok(())
}

/// Read a [`MarketRecording`] from a zstd-compressed JSON file.
///
/// # Errors
///
/// Fails when the file is missing, is not zstd-compressed JSON, or has a schema version other than
/// [`SCHEMA_VERSION`]. A version 1 file holds decoded states, which version 2 replaces; record it
/// again with tools/record-market.
pub fn read_recording(path: &Path) -> anyhow::Result<MarketRecording> {
    /// The version alone, so an old file is refused before its body is parsed as this format.
    #[derive(Deserialize)]
    struct VersionOnly {
        metadata: MetadataVersion,
    }
    #[derive(Deserialize)]
    struct MetadataVersion {
        #[serde(default)]
        schema_version: u32,
    }

    let compressed = std::fs::read(path)?;
    let decompressed = zstd::decode_all(compressed.as_slice())?;
    let schema_version = serde_json::from_slice::<VersionOnly>(&decompressed)?
        .metadata
        .schema_version;
    if schema_version != SCHEMA_VERSION {
        anyhow::bail!(
            "{} is recording schema version {schema_version}, but this build reads version \
             {SCHEMA_VERSION}; record it again with tools/record-market",
            path.display()
        );
    }
    Ok(serde_json::from_slice(&decompressed)?)
}

/// Compute SHA-256 hex digest of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(schema_version: u32) -> MarketRecording {
        MarketRecording {
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
                schema_version,
            },
            tokens: Vec::new(),
            messages: vec![dto::FeedMessage::default()],
        }
    }

    #[test]
    fn test_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json.zst");

        write_recording(&recording(SCHEMA_VERSION), &path).unwrap();
        let loaded = read_recording(&path).unwrap();

        assert_eq!(loaded.metadata.chain, "ethereum");
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn test_read_other_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.json.zst");
        write_recording(&recording(1), &path).unwrap();

        let error = read_recording(&path)
            .unwrap_err()
            .to_string();

        assert!(error.contains("schema version 1"), "{error}");
        assert!(error.contains("record it again"), "{error}");
    }
}
