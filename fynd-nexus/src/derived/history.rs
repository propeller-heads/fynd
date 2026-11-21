//! The history of one derived kind: its latest output, the changes of its recent publishes and
//! a cached full encoding.

use std::{
    collections::{HashMap, VecDeque},
    io::Write as _,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use bytes::Bytes;
use flate2::{write::GzEncoder, Compression};
use fynd_core::nexus::derived::{
    DerivedChanges, DerivedCursor, DerivedUpdate, ServedDerivedOutput,
};
use metrics::{gauge, histogram, Label};
use tokio::task::JoinError;

use crate::derived::models::{DerivedAnswer, EncodedFullUpdate};

/// How many publishes of a kind are kept as changes. A reader whose cursor is older than that
/// gets a full update. At 0.45 s per block it covers about two minutes.
const CHANGE_HISTORY_LEN: usize = 256;

/// How long an encoded full update is served before the next request encodes a new one. Readers
/// catch up from it with a delta, which the history must still cover.
const FULL_UPDATE_MAX_AGE: Duration = Duration::from_secs(5);

/// What a publish changed in the history of a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishSummary {
    /// The position the publish brought the kind to.
    pub seq: u64,
    /// The block of the publish.
    pub block: u64,
    /// How many entries the publish added or changed.
    pub upserted: usize,
    /// How many entries the publish removed.
    pub removed: usize,
    /// How many spot prices the publish left out for not being finite.
    pub non_finite_left_out: usize,
}

/// The changes that brought the kind to `seq`.
struct PublishedChanges<Output: ServedDerivedOutput> {
    seq: u64,
    changes: DerivedChanges<Output>,
}

struct HistoryState<Output: ServedDerivedOutput> {
    latest: Arc<Output>,
    /// The position of the latest publish; 0 while nothing was published.
    seq: u64,
    block: u64,
    /// The oldest position a delta can start from.
    oldest_delta_start_seq: u64,
    /// The changes of the most recent publishes after the first, oldest first.
    changes: VecDeque<PublishedChanges<Output>>,
}

struct CachedFullUpdate {
    update: EncodedFullUpdate,
    encoded_at: Instant,
}

/// The latest output of one derived kind and the means to serve it: full updates and deltas.
///
/// [`publish`](Self::publish) is called by one writer at a time.
pub struct DerivedHistory<Output: ServedDerivedOutput> {
    run_id: u64,
    labels: Vec<Label>,
    state: Mutex<HistoryState<Output>>,
    /// Holds the encoded full update. A request that finds it too old encodes a new one while
    /// holding the lock, so concurrent requests wait for it instead of encoding too.
    full_update: tokio::sync::Mutex<Option<CachedFullUpdate>>,
}

impl<Output: ServedDerivedOutput> DerivedHistory<Output> {
    /// An empty history for the service run `run_id`. Its metrics carry `labels`.
    pub fn new(run_id: u64, labels: Vec<Label>) -> Self {
        let state = HistoryState {
            latest: Arc::new(Output::default()),
            seq: 0,
            block: 0,
            oldest_delta_start_seq: 0,
            changes: VecDeque::new(),
        };
        Self { run_id, labels, state: Mutex::new(state), full_update: Default::default() }
    }

    /// The block of the latest publish, or `None` while nothing was published.
    pub fn block(&self) -> Option<u64> {
        let state = self.lock_state();
        (state.seq > 0).then_some(state.block)
    }

    /// Makes `current`, computed at `block`, the latest output, and returns what changed.
    ///
    /// Returns `None` and publishes nothing when `current` equals the latest output and `block`
    /// is the block of the latest publish. Equal data at another block publishes an empty delta,
    /// and different data at the same block, as after a reorg, publishes its changes. The first
    /// publish always happens.
    pub fn publish(&self, current: &Output, block: u64) -> Option<PublishSummary> {
        let (previous, previous_seq, previous_block) = {
            let state = self.lock_state();
            (Arc::clone(&state.latest), state.seq, state.block)
        };
        let changes = current.changes_since(&previous);
        drop(previous);
        if previous_seq > 0 && changes.is_empty() && previous_block == block {
            return None;
        }
        let summary = PublishSummary {
            seq: previous_seq + 1,
            block,
            upserted: changes.upserted.len(),
            removed: changes.removed.len(),
            non_finite_left_out: changes.non_finite_left_out,
        };
        let mut state = self.lock_state();
        Arc::make_mut(&mut state.latest).apply(&changes);
        state.seq = summary.seq;
        state.block = block;
        if summary.seq == 1 {
            state.oldest_delta_start_seq = 1;
        } else {
            state
                .changes
                .push_back(PublishedChanges { seq: summary.seq, changes });
            if state.changes.len() > CHANGE_HISTORY_LEN {
                if let Some(dropped) = state.changes.pop_front() {
                    state.oldest_delta_start_seq = dropped.seq;
                }
            }
        }
        drop(state);
        gauge!("nexus_derived_block", self.labels.clone()).set(block as f64);
        Some(summary)
    }

    /// The answer to a reader at `since` that may also send the cursor of its `ETag` as
    /// `if_none_match`.
    ///
    /// It is a 304 when either equals the latest cursor, a delta when `since` is of this run and
    /// still covered by the history, and a full update otherwise. A full update is encoded on the
    /// blocking pool and reused until it is [`FULL_UPDATE_MAX_AGE`] old.
    ///
    /// # Errors
    ///
    /// Fails when the task that encodes the full update does not finish.
    pub async fn answer(
        &self,
        since: Option<DerivedCursor>,
        if_none_match: Option<DerivedCursor>,
    ) -> Result<DerivedAnswer, JoinError> {
        let merged = {
            let state = self.lock_state();
            if state.seq == 0 {
                return Ok(DerivedAnswer::NoData);
            }
            let latest = DerivedCursor { run_id: self.run_id, seq: state.seq };
            if since == Some(latest) || if_none_match == Some(latest) {
                return Ok(DerivedAnswer::NotModified(latest));
            }
            since
                .filter(|since| since.run_id == self.run_id)
                .and_then(|since| {
                    let DerivedChanges { upserted, removed, .. } =
                        state.merged_changes_since(since.seq)?;
                    Some(MergedDelta::<Output> {
                        from_seq: since.seq,
                        cursor: latest,
                        block: state.block,
                        upserted,
                        removed,
                    })
                })
        };
        let Some(merged) = merged else {
            return self
                .full_update()
                .await
                .map(DerivedAnswer::Full);
        };
        let entries = merged.upserted.len() + merged.removed.len();
        let cursor = merged.cursor;
        let delta = DerivedUpdate::<Output::Entry, Output::Key>::Delta {
            run_id: cursor.run_id,
            from_seq: merged.from_seq,
            seq: cursor.seq,
            block: merged.block,
            upserted: merged.upserted,
            removed: merged.removed,
        };
        let json = serde_json::to_vec(&delta)
            .expect("a derived delta of serializable entries encodes to JSON");
        histogram!("nexus_derived_delta_bytes", self.labels.clone()).record(json.len() as f64);
        histogram!("nexus_derived_delta_entries", self.labels.clone()).record(entries as f64);
        Ok(DerivedAnswer::Delta { cursor, json: json.into() })
    }

    async fn full_update(&self) -> Result<EncodedFullUpdate, JoinError> {
        let mut cached = self.full_update.lock().await;
        if let Some(cached) = cached
            .as_ref()
            .filter(|cached| cached.encoded_at.elapsed() < FULL_UPDATE_MAX_AGE)
        {
            return Ok(cached.update.clone());
        }
        let (output, seq, block) = {
            let state = self.lock_state();
            (Arc::clone(&state.latest), state.seq, state.block)
        };
        let (run_id, labels) = (self.run_id, self.labels.clone());
        let update = tokio::task::spawn_blocking(move || {
            encode_full_update(output, DerivedCursor { run_id, seq }, block, &labels)
        })
        .await?;
        *cached = Some(CachedFullUpdate { update: update.clone(), encoded_at: Instant::now() });
        Ok(update)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HistoryState<Output>> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<Output: ServedDerivedOutput> HistoryState<Output> {
    /// The entries and keys that bring a reader at `since_seq` to the latest publish, or `None`
    /// when the history does not cover `since_seq`. The last write of a key wins, so a key
    /// upserted and then removed ends as removed, and the reverse as upserted.
    fn merged_changes_since(&self, since_seq: u64) -> Option<DerivedChanges<Output>> {
        if since_seq < self.oldest_delta_start_seq || since_seq >= self.seq {
            return None;
        }
        let mut last_write: HashMap<Output::Key, Option<Output::Entry>> = HashMap::new();
        for published in self
            .changes
            .iter()
            .filter(|published| published.seq > since_seq)
        {
            for key in &published.changes.removed {
                last_write.insert(key.clone(), None);
            }
            for entry in &published.changes.upserted {
                last_write.insert(Output::entry_key(entry), Some(entry.clone()));
            }
        }
        let mut upserted = Vec::new();
        let mut removed = Vec::new();
        for (key, write) in last_write {
            match write {
                Some(entry) => upserted.push(entry),
                None => removed.push(key),
            }
        }
        Some(DerivedChanges { upserted, removed, non_finite_left_out: 0 })
    }
}

/// The merged changes of several publishes, as the answer to one reader.
struct MergedDelta<Output: ServedDerivedOutput> {
    from_seq: u64,
    cursor: DerivedCursor,
    block: u64,
    upserted: Vec<Output::Entry>,
    removed: Vec<Output::Key>,
}

/// Encodes `output`, releasing it as soon as its entries are copied so that a publish in the
/// meantime does not have to copy the whole output.
fn encode_full_update<Output: ServedDerivedOutput>(
    output: Arc<Output>,
    cursor: DerivedCursor,
    block: u64,
    labels: &[Label],
) -> EncodedFullUpdate {
    let entries = output.entries();
    drop(output);
    let update = DerivedUpdate::<Output::Entry, Output::Key>::Full {
        run_id: cursor.run_id,
        seq: cursor.seq,
        block,
        entries,
    };
    let json = serde_json::to_vec(&update)
        .expect("a full derived update of serializable entries encodes to JSON");
    let gzip = gzip(&json);
    for (encoding, bytes) in [("identity", json.len()), ("gzip", gzip.len())] {
        let mut labels = labels.to_vec();
        labels.push(Label::new("encoding", encoding));
        histogram!("nexus_derived_snapshot_bytes", labels).record(bytes as f64);
    }
    EncodedFullUpdate { cursor, json: json.into(), gzip }
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
