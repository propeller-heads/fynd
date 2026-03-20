use std::path::Path;

use crate::recording::types::MarketRecording;

/// Serializes a [`MarketRecording`] to a zstd-compressed JSON file.
pub fn write_recording(recording: &MarketRecording, path: &Path) -> anyhow::Result<()> {
    let json = serde_json::to_vec(recording)?;
    let compressed = zstd::encode_all(json.as_slice(), 3)?;
    std::fs::write(path, compressed)?;
    Ok(())
}

/// Reads a [`MarketRecording`] from a zstd-compressed JSON file.
pub fn read_recording(path: &Path) -> anyhow::Result<MarketRecording> {
    let compressed = std::fs::read(path)?;
    let decompressed = zstd::decode_all(compressed.as_slice())?;
    let recording: MarketRecording = serde_json::from_slice(&decompressed)?;
    Ok(recording)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::types::{RecordedUpdate, RecordingMetadata};
    use std::collections::HashMap;

    fn make_empty_recording() -> MarketRecording {
        MarketRecording {
            metadata: RecordingMetadata {
                chain: "ethereum".to_string(),
                recorded_at_unix_s: 1710000000,
                fynd_version: "0.19.0".to_string(),
                recording_duration_s: 600,
                num_updates: 1,
                protocols: vec!["uniswap_v2".to_string()],
                min_tvl: 10.0,
                min_token_quality: 100,
                traded_n_days_ago: Some(3),
                gas_price_gwei: Some(15.0),
            },
            updates: vec![RecordedUpdate {
                block_number_or_timestamp: 12345,
                sync_states: HashMap::new(),
                states: HashMap::new(),
                new_pairs: HashMap::new(),
                removed_pairs: HashMap::new(),
            }],
        }
    }

    #[test]
    fn test_write_read_roundtrip_empty() {
        let recording = make_empty_recording();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_recording.json.zst");

        write_recording(&recording, &path).unwrap();
        let loaded = read_recording(&path).unwrap();

        assert_eq!(loaded.metadata.chain, "ethereum");
        assert_eq!(loaded.metadata.num_updates, 1);
        assert_eq!(loaded.metadata.protocols, vec!["uniswap_v2"]);
        assert_eq!(loaded.updates.len(), 1);
        assert_eq!(loaded.updates[0].block_number_or_timestamp, 12345);
    }

    #[test]
    fn test_write_read_roundtrip_with_protocol_sim() {
        use crate::algorithm::test_utils::MockProtocolSim;
        use tycho_simulation::tycho_core::simulation::protocol_sim::ProtocolSim;

        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        states.insert("pool_1".to_string(), Box::new(MockProtocolSim::new(2000.0)));

        let recording = MarketRecording {
            metadata: RecordingMetadata {
                chain: "ethereum".to_string(),
                recorded_at_unix_s: 1710000000,
                fynd_version: "0.19.0".to_string(),
                recording_duration_s: 600,
                num_updates: 1,
                protocols: vec![],
                min_tvl: 0.0,
                min_token_quality: 100,
                traded_n_days_ago: None,
                gas_price_gwei: None,
            },
            updates: vec![RecordedUpdate {
                block_number_or_timestamp: 12345,
                sync_states: HashMap::new(),
                states,
                new_pairs: HashMap::new(),
                removed_pairs: HashMap::new(),
            }],
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_with_sim.json.zst");

        write_recording(&recording, &path).unwrap();
        let loaded = read_recording(&path).unwrap();

        assert_eq!(loaded.updates[0].states.len(), 1);
        assert!(loaded.updates[0].states.contains_key("pool_1"));
    }
}
