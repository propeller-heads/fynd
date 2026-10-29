//! State that pods share through a `fynd-nexus` service, and the messages that carry it.
//!
//! The service holds one connection per source and keeps the latest state of each in memory. Pods
//! poll it over HTTP. Each response carries a [`NexusMessage`] as JSON.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// The protocol version this build writes and reads.
pub const PROTOCOL_VERSION: u32 = 1;

/// The current message of a nexus stream, for state of type `D`.
///
/// `epoch` changes each time the service starts and `seq` grows per stream, so a reader can tell a
/// replayed or reordered message from a new one. In JSON the version is the field `v` and the
/// body's fields sit beside `epoch` and `seq`, with `type` naming the kind of body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NexusMessage<D> {
    /// The protocol version the message was written in.
    #[serde(rename = "v")]
    pub version: u32,
    /// Identifies the service run that sent the message.
    pub epoch: u64,
    /// The position of the message in its stream.
    pub seq: u64,
    /// The snapshot or withdrawal carried by the message.
    #[serde(flatten)]
    pub body: MessageBody<D>,
}

/// A snapshot or withdrawal carried by a [`NexusMessage`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageBody<D> {
    /// The current state.
    Snapshot {
        /// When the service got the state from its source.
        as_of: DateTime<Utc>,
        /// The state.
        data: D,
    },
    /// The service has no usable state: what the source sent went stale, or its feed ended.
    Withdrawn,
}

/// Errors when decoding text as a [`NexusMessage`].
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    /// The text is not a message of this protocol.
    #[error("malformed nexus message: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The message uses a protocol version this build does not support.
    #[error("unsupported nexus protocol version {found}; this build reads {supported}")]
    UnsupportedVersion {
        /// The version in the message.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
}

/// Reads only the protocol version so decoding can report an unsupported version even when the
/// message body cannot be parsed.
#[derive(Deserialize)]
struct VersionField {
    v: u32,
}

impl<D> NexusMessage<D> {
    /// A message in this build's protocol version.
    pub fn new(epoch: u64, seq: u64, body: MessageBody<D>) -> Self {
        Self { version: PROTOCOL_VERSION, epoch, seq, body }
    }
}

impl<D: Serialize> NexusMessage<D> {
    /// The JSON text of this message.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl<D: DeserializeOwned> NexusMessage<D> {
    /// Reads a message written by [`encode`](Self::encode).
    ///
    /// # Errors
    ///
    /// [`MessageError::UnsupportedVersion`] for an unsupported protocol version, and
    /// [`MessageError::Malformed`] when the text does not match this version's message format.
    pub fn decode(text: &str) -> Result<Self, MessageError> {
        let unsupported =
            |found| MessageError::UnsupportedVersion { found, supported: PROTOCOL_VERSION };
        match serde_json::from_str::<Self>(text) {
            Ok(message) if message.version == PROTOCOL_VERSION => Ok(message),
            Ok(message) => Err(unsupported(message.version)),
            Err(error) => {
                let found = serde_json::from_str::<VersionField>(text)
                    .ok()
                    .map(|field| field.v)
                    .filter(|version| *version != PROTOCOL_VERSION);
                Err(match found {
                    Some(version) => unsupported(version),
                    None => MessageError::Malformed(error),
                })
            }
        }
    }
}
