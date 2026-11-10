//! The latest snapshot of each derived computation, kept in memory for the API to serve.

use std::{
    collections::HashMap,
    io::Write as _,
    sync::{PoisonError, RwLock},
};

use bytes::Bytes;
use flate2::{write::GzEncoder, Compression};
use fynd_core::nexus::derived::{DerivedKind, DerivedSnapshot};
use metrics::{gauge, histogram, Label};
use serde::Serialize;
use tycho_simulation::evm::tycho_models::Chain;

use crate::derived::models::EncodedDerivedSnapshot;

/// The latest snapshot of each derived computation.
pub struct DerivedSnapshots {
    chain: Chain,
    latest: RwLock<HashMap<DerivedKind, EncodedDerivedSnapshot>>,
}

impl DerivedSnapshots {
    /// An empty store for the service of `chain`.
    pub fn new(chain: Chain) -> Self {
        Self { chain, latest: RwLock::new(HashMap::new()) }
    }

    /// The `chain` and `computation` label values every metric of `kind` carries.
    pub fn labels(&self, kind: DerivedKind) -> Vec<Label> {
        vec![Label::new("chain", self.chain.to_string()), Label::new("computation", kind.name())]
    }

    /// Replaces the latest snapshot of `kind` with `snapshot`, encoding it outside the lock.
    pub fn replace<E: Serialize>(&self, kind: DerivedKind, snapshot: &DerivedSnapshot<E>) {
        let json = serde_json::to_vec(snapshot)
            .expect("a derived snapshot of serializable entries encodes to JSON");
        let gzip = gzip(&json);
        self.record_bytes(kind, "identity", json.len());
        self.record_bytes(kind, "gzip", gzip.len());
        gauge!("nexus_derived_block", self.labels(kind)).set(snapshot.block as f64);
        let encoded = EncodedDerivedSnapshot { block: snapshot.block, json: json.into(), gzip };
        self.latest
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind, encoded);
    }

    /// The latest snapshot of `kind`, or `None` while none has been written.
    pub fn current(&self, kind: DerivedKind) -> Option<EncodedDerivedSnapshot> {
        self.latest
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kind)
            .cloned()
    }

    /// The block of the latest snapshot of `kind`, or `None` while none has been written.
    pub fn block(&self, kind: DerivedKind) -> Option<u64> {
        self.latest
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kind)
            .map(|encoded| encoded.block)
    }

    fn record_bytes(&self, kind: DerivedKind, encoding: &'static str, bytes: usize) {
        let mut labels = self.labels(kind);
        labels.push(Label::new("encoding", encoding));
        histogram!("nexus_derived_snapshot_bytes", labels).record(bytes as f64);
    }
}

fn gzip(plain: &[u8]) -> Bytes {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(plain)
        .expect("writing to a vector cannot fail");
    Bytes::from(
        encoder
            .finish()
            .expect("finishing a gzip stream into a vector cannot fail"),
    )
}
