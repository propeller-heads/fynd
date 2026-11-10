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

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use fynd_core::nexus::derived::SpotPriceEntry;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use serde_json::{json, Value};

    use super::*;

    fn snapshot(block: u64) -> DerivedSnapshot<Value> {
        DerivedSnapshot { block, entries: vec![json!({"n": block})] }
    }

    #[test]
    fn test_nothing_is_current_before_the_first_replace() {
        let snapshots = DerivedSnapshots::new(Chain::Base);

        assert!(snapshots
            .current(DerivedKind::PoolDepths)
            .is_none());
        assert_eq!(snapshots.block(DerivedKind::PoolDepths), None);
    }

    #[test]
    fn test_replace_replaces_only_its_kind_and_encodes_both_forms() {
        let snapshots = DerivedSnapshots::new(Chain::Base);
        snapshots.replace(DerivedKind::PoolDepths, &snapshot(5));
        snapshots.replace(DerivedKind::PoolDepths, &snapshot(6));

        let current = snapshots
            .current(DerivedKind::PoolDepths)
            .unwrap();

        assert_eq!(current.block, 6);
        assert_eq!(
            serde_json::from_slice::<Value>(&current.json).unwrap(),
            json!({"block": 6, "entries": [{"n": 6}]})
        );
        let mut decompressed = Vec::new();
        flate2::read::GzDecoder::new(&current.gzip[..])
            .read_to_end(&mut decompressed)
            .unwrap();
        assert_eq!(decompressed, current.json);
        assert_eq!(snapshots.block(DerivedKind::SpotPrices), None);
    }

    #[test]
    fn test_replace_records_sizes_and_block() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let snapshots = DerivedSnapshots::new(Chain::Base);

        metrics::with_local_recorder(&recorder, || {
            snapshots.replace(
                DerivedKind::SpotPrices,
                &DerivedSnapshot::<SpotPriceEntry> { block: 42, entries: vec![] },
            );
        });

        let current = snapshots
            .current(DerivedKind::SpotPrices)
            .unwrap();
        let mut recorded = Vec::new();
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let label = |name: &str| {
                key.key()
                    .labels()
                    .find(|label| label.key() == name)
                    .map(|label| label.value().to_string())
            };
            assert_eq!(label("computation").as_deref(), Some("spot_prices"));
            let value = match value {
                DebugValue::Histogram(sizes) => sizes[0].into_inner(),
                DebugValue::Gauge(block) => block.into_inner(),
                DebugValue::Counter(_) => panic!("unexpected counter"),
            };
            recorded.push((key.key().name().to_string(), label("encoding"), value as usize));
        }
        recorded.sort();
        assert_eq!(
            recorded,
            [
                ("nexus_derived_block".to_string(), None, 42),
                (
                    "nexus_derived_snapshot_bytes".to_string(),
                    Some("gzip".to_string()),
                    current.gzip.len()
                ),
                (
                    "nexus_derived_snapshot_bytes".to_string(),
                    Some("identity".to_string()),
                    current.json.len()
                ),
            ]
        );
    }
}
