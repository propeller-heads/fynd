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
        Self {
            block_number_or_timestamp: update.block_number_or_timestamp,
            sync_states: update.sync_states,
            states: update.states,
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
    pub chain_id: u64,
    pub recorded_at_unix_s: u64,
    pub fynd_version: String,
    pub recording_duration_s: u64,
    pub num_updates: usize,
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
