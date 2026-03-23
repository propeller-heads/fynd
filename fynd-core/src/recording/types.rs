use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tycho_simulation::{
    protocol::models::{ProtocolComponent, Update},
    tycho_client::feed::SynchronizerState,
    tycho_core::simulation::protocol_sim::ProtocolSim,
};

/// A serializable mirror of [`Update`].
///
/// `Update` itself is `#[derive(Debug, Clone)]` only. All its fields
/// implement `Serialize`/`Deserialize` individually (`Box<dyn ProtocolSim>`
/// via `#[typetag::serde]`), so this wrapper just adds the derives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedUpdate {
    pub block_number_or_timestamp: u64,
    pub sync_states: HashMap<String, SynchronizerState>,
    pub states: HashMap<String, Box<dyn ProtocolSim>>,
    pub new_pairs: HashMap<String, ProtocolComponent>,
    pub removed_pairs: HashMap<String, ProtocolComponent>,
}

impl From<Update> for RecordedUpdate {
    fn from(update: Update) -> Self {
        // Filter out states that can't be serialized (e.g., VM-backed
        // protocol states like UniswapV4 which depend on EVM engine state).
        // This works because `#[typetag::serde]` dispatches to each
        // concrete type's Serialize impl — VM-backed states return
        // `Err(serde::ser::Error::custom("not supported due vm state deps"))`.
        // Note: `new_pairs` still registers these pools as components, but
        // without a simulation state they can't compute spot prices.
        let states = update
            .states
            .into_iter()
            .filter(|(id, state)| {
                let ok = serde_json::to_value(state.as_ref()).is_ok();
                if !ok {
                    tracing::debug!(
                        pool_id = %id,
                        "dropping non-serializable state from recording"
                    );
                }
                ok
            })
            .collect();

        Self {
            block_number_or_timestamp: update.block_number_or_timestamp,
            sync_states: update.sync_states,
            states,
            new_pairs: update.new_pairs,
            removed_pairs: update.removed_pairs,
        }
    }
}

impl From<RecordedUpdate> for Update {
    fn from(rec: RecordedUpdate) -> Self {
        Update::new(
            rec.block_number_or_timestamp,
            rec.states,
            rec.new_pairs,
        )
        .set_removed_pairs(rec.removed_pairs)
        .set_sync_states(rec.sync_states)
    }
}

/// Metadata about the recording session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingMetadata {
    pub chain: String,
    pub recorded_at_unix_s: u64,
    pub fynd_version: String,
    pub recording_duration_s: u64,
    pub num_updates: usize,
    pub protocols: Vec<String>,
    pub min_tvl: f64,
    pub min_token_quality: i32,
    pub traded_n_days_ago: Option<u64>,
    /// Gas price in wei captured from RPC at recording time.
    /// Stored as a decimal string to preserve full precision.
    /// Used during replay to avoid needing a live RPC connection.
    #[serde(default)]
    pub gas_price_wei: Option<String>,
}

/// A complete market recording: metadata + ordered sequence of `Update` messages.
///
/// Captures the raw Tycho stream output so integration tests can replay
/// through `TychoFeed::handle_tycho_message()` — the same ingestion path
/// used in production.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRecording {
    pub metadata: RecordingMetadata,
    pub updates: Vec<RecordedUpdate>,
}

impl MarketRecording {
    /// Returns the block number from the last recorded update, or 0 if empty.
    pub fn last_block_number(&self) -> u64 {
        self.updates
            .last()
            .map(|u| u.block_number_or_timestamp)
            .unwrap_or(0)
    }
}
