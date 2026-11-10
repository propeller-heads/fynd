//! The encoded derived data the API serves.

use bytes::Bytes;

/// A derived snapshot as JSON, plain and gzip-compressed, each encoded once when written.
#[derive(Debug, Clone)]
pub struct EncodedDerivedSnapshot {
    /// The block the snapshot was computed at.
    pub block: u64,
    /// The snapshot as JSON. Clones share one buffer.
    pub json: Bytes,
    /// `json` compressed with gzip. Clones share one buffer.
    pub gzip: Bytes,
}

impl EncodedDerivedSnapshot {
    /// The HTTP `ETag` of this snapshot, such as `"21000000"`.
    pub fn etag(&self) -> String {
        format!("\"{}\"", self.block)
    }
}
