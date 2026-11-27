//! What the API serves for a derived kind.

use bytes::Bytes;
use fynd_core::nexus::derived::DerivedCursor;

/// A full update of a derived kind as JSON, plain and gzip-compressed, each encoded once.
#[derive(Debug, Clone)]
pub struct EncodedFullUpdate {
    /// The position in the kind's history that the update brings a reader to.
    pub cursor: DerivedCursor,
    /// The update as JSON. Clones share one buffer.
    pub json: Bytes,
    /// `json` compressed with gzip. Clones share one buffer.
    pub gzip: Bytes,
}

/// What the service answers to a request for a derived kind.
#[derive(Debug, Clone)]
pub enum DerivedAnswer {
    /// The kind has no data yet.
    NoData,
    /// The reader already holds the latest update.
    NotModified(DerivedCursor),
    /// What changed between the reader's cursor and the latest update, as JSON.
    Delta {
        /// The latest update.
        cursor: DerivedCursor,
        /// The delta as JSON.
        json: Bytes,
    },
    /// The whole kind.
    Full(EncodedFullUpdate),
}
