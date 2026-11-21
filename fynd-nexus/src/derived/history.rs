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

#[cfg(test)]
mod tests {
    use fynd_core::{
        derived::types::SpotPrices,
        nexus::derived::{DirectionEntryKey, SpotPriceEntry},
    };
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use rstest::rstest;
    use tycho_simulation::tycho_core::Bytes as Address;

    use super::*;

    const RUN_ID: u64 = 1_761_550_000_000;

    type SpotDelta = DerivedUpdate<SpotPriceEntry, DirectionEntryKey>;

    fn spot(prices: &[(&str, f64)]) -> SpotPrices {
        let mut output = SpotPrices::default();
        for (component, price) in prices {
            output.insert(
                (component.to_string(), Address::from([1; 20]), Address::from([2; 20])),
                *price,
            );
        }
        output
    }

    fn history() -> DerivedHistory<SpotPrices> {
        DerivedHistory::new(RUN_ID, Vec::new())
    }

    fn cursor(seq: u64) -> DerivedCursor {
        DerivedCursor { run_id: RUN_ID, seq }
    }

    /// A history that published each of `states` at the blocks 10, 11, ...
    fn published<State: AsRef<[(&'static str, f64)]>>(
        states: &[State],
    ) -> DerivedHistory<SpotPrices> {
        let history = history();
        for (block, state) in (10..).zip(states) {
            history.publish(&spot(state.as_ref()), block);
        }
        history
    }

    fn delta_of(answer: DerivedAnswer) -> SpotDelta {
        let DerivedAnswer::Delta { json, .. } = answer else {
            panic!("expected a delta, got {answer:?}");
        };
        serde_json::from_slice(&json).unwrap()
    }

    fn answer_kind(answer: &DerivedAnswer) -> &'static str {
        match answer {
            DerivedAnswer::NoData => "no_data",
            DerivedAnswer::NotModified(_) => "not_modified",
            DerivedAnswer::Delta { .. } => "delta",
            DerivedAnswer::Full(_) => "full",
        }
    }

    #[rstest]
    #[case::last_write_wins(
        vec![vec![("a", 1.0)], vec![("a", 2.0)], vec![("a", 3.0)]],
        &[("a", 3.0)],
        &[]
    )]
    #[case::upsert_then_remove(
        vec![vec![("a", 1.0)], vec![("a", 1.0), ("b", 2.0)], vec![("a", 1.0)]],
        &[],
        &["b"]
    )]
    #[case::remove_then_upsert(
        vec![vec![("a", 1.0), ("b", 2.0)], vec![("a", 1.0)], vec![("a", 1.0), ("b", 5.0)]],
        &[("b", 5.0)],
        &[]
    )]
    #[case::independent_keys(
        vec![vec![("a", 1.0), ("b", 2.0)], vec![("a", 1.5), ("b", 2.0)], vec![("a", 1.5), ("c", 4.0)]],
        &[("a", 1.5), ("c", 4.0)],
        &["b"]
    )]
    #[tokio::test]
    async fn test_merged_delta(
        #[case] states: Vec<Vec<(&'static str, f64)>>,
        #[case] upserted: &[(&str, f64)],
        #[case] removed: &[&str],
    ) {
        let history = published(&states);

        let answer = history
            .answer(Some(cursor(1)), None)
            .await
            .unwrap();

        let SpotDelta::Delta {
            from_seq,
            seq,
            upserted: actual_upserted,
            removed: actual_removed,
            ..
        } = delta_of(answer)
        else {
            panic!("expected a delta");
        };
        let mut upserted_prices: Vec<_> = actual_upserted
            .iter()
            .map(|entry| (entry.component_id.as_str(), entry.price))
            .collect();
        upserted_prices.sort_by(|left, right| left.0.cmp(right.0));
        let mut removed_ids: Vec<_> = actual_removed
            .iter()
            .map(|key| key.component_id.as_str())
            .collect();
        removed_ids.sort_unstable();
        assert_eq!((from_seq, seq), (1, states.len() as u64));
        assert_eq!(upserted_prices, upserted);
        assert_eq!(removed_ids, removed);
    }

    #[rstest]
    #[case::no_cursor(None, None, "full")]
    #[case::since_latest(Some(cursor(3)), None, "not_modified")]
    #[case::etag_of_latest(None, Some(cursor(3)), "not_modified")]
    #[case::since_covered(Some(cursor(1)), None, "delta")]
    #[case::since_one_behind(Some(cursor(2)), None, "delta")]
    #[case::since_before_the_first_publish(Some(cursor(0)), None, "full")]
    #[case::since_ahead_of_latest(Some(cursor(9)), None, "full")]
    #[case::etag_of_an_older_update(None, Some(cursor(2)), "full")]
    #[case::other_run(Some(DerivedCursor { run_id: RUN_ID + 1, seq: 2 }), None, "full")]
    #[tokio::test]
    async fn test_answer_selection(
        #[case] since: Option<DerivedCursor>,
        #[case] if_none_match: Option<DerivedCursor>,
        #[case] expected: &str,
    ) {
        let history = published(&[&[("a", 1.0)], &[("a", 2.0)], &[("a", 3.0)]]);

        let answer = history
            .answer(since, if_none_match)
            .await
            .unwrap();

        assert_eq!(answer_kind(&answer), expected);
    }

    #[tokio::test]
    async fn test_answer_without_a_publish() {
        let answer = history()
            .answer(None, None)
            .await
            .unwrap();

        assert_eq!(answer_kind(&answer), "no_data");
    }

    #[rstest]
    #[case::oldest_covered(44, "delta")]
    #[case::just_outside(43, "full")]
    #[tokio::test]
    async fn test_answer_at_the_edge_of_the_change_history(
        #[case] since_seq: u64,
        #[case] expected: &str,
    ) {
        let history = history();
        for block in 1..=300u64 {
            history.publish(&spot(&[("a", block as f64)]), block);
        }

        let answer = history
            .answer(Some(cursor(since_seq)), None)
            .await
            .unwrap();

        assert_eq!(answer_kind(&answer), expected);
    }

    #[rstest]
    #[case::same_block_same_data(&[("a", 1.0)], 10, None)]
    #[case::same_block_changed_data(&[("a", 2.0)], 10, Some((1, 0, 10)))]
    #[case::new_block_same_data(&[("a", 1.0)], 11, Some((0, 0, 11)))]
    #[case::lower_block_same_data(&[("a", 1.0)], 9, Some((0, 0, 9)))]
    #[case::new_block_data_removed(&[], 11, Some((0, 1, 11)))]
    fn test_publish_after_a_block_at_10(
        #[case] state: &[(&str, f64)],
        #[case] block: u64,
        #[case] published_as: Option<(usize, usize, u64)>,
    ) {
        let history = published(&[&[("a", 1.0)]]);

        let summary = history.publish(&spot(state), block);

        let actual = summary.map(|summary| (summary.upserted, summary.removed, summary.block));
        assert_eq!(actual, published_as);
        assert_eq!(history.block(), Some(published_as.map_or(10, |(_, _, block)| block)));
    }

    #[test]
    fn test_first_publish_of_an_empty_output() {
        let history = history();

        let summary = history.publish(&spot(&[]), 10);

        assert_eq!(summary.map(|summary| summary.seq), Some(1));
    }

    #[tokio::test]
    async fn test_empty_delta_carries_the_new_block() {
        let history = published(&[&[("a", 1.0)]]);
        history.publish(&spot(&[("a", 1.0)]), 11);

        let answer = history
            .answer(Some(cursor(1)), None)
            .await
            .unwrap();

        let SpotDelta::Delta { block, upserted, removed, .. } = delta_of(answer) else {
            panic!("expected a delta");
        };
        assert_eq!((block, upserted.len(), removed.len()), (11, 0, 0));
    }

    #[test]
    fn test_non_finite_prices_are_left_out_and_counted() {
        let history = history();

        let summary = history
            .publish(&spot(&[("a", 1.0), ("b", f64::NAN)]), 10)
            .unwrap();

        assert_eq!((summary.upserted, summary.non_finite_left_out), (1, 1));
    }

    #[tokio::test]
    async fn test_full_update_is_served_while_fresh_and_catchable_by_delta() {
        let history = published(&[&[("a", 1.0)]]);
        let DerivedAnswer::Full(first) = history
            .answer(None, None)
            .await
            .unwrap()
        else {
            panic!("expected a full update");
        };
        history.publish(&spot(&[("a", 2.0)]), 11);

        let answer = history
            .answer(None, None)
            .await
            .unwrap();
        let catch_up = history
            .answer(Some(first.cursor), None)
            .await
            .unwrap();

        let DerivedAnswer::Full(second) = answer else {
            panic!("expected a full update");
        };
        assert_eq!(second.json.as_ptr(), first.json.as_ptr(), "the encoding is reused");
        assert_eq!(second.cursor, cursor(1));
        assert_eq!(answer_kind(&catch_up), "delta");
    }

    #[tokio::test]
    async fn test_concurrent_requests_share_one_full_encoding() {
        let history = published(&[&[("a", 1.0)]]);

        let answers = futures::future::join_all((0..8).map(|_| history.answer(None, None))).await;

        let pointers: std::collections::HashSet<_> = answers
            .into_iter()
            .map(|answer| match answer.unwrap() {
                DerivedAnswer::Full(full) => full.json.as_ptr(),
                other => panic!("expected a full update, got {other:?}"),
            })
            .collect();
        assert_eq!(pointers.len(), 1);
    }

    #[tokio::test]
    async fn test_full_update_is_a_snapshot_of_the_latest_output() {
        let history = published(&[vec![("a", 1.0)], vec![("a", 1.0), ("b", 2.0)]]);

        let DerivedAnswer::Full(full) = history
            .answer(None, None)
            .await
            .unwrap()
        else {
            panic!("expected a full update");
        };

        let SpotDelta::Full { run_id, seq, block, entries } =
            serde_json::from_slice(&full.json).unwrap()
        else {
            panic!("expected a full update");
        };
        assert_eq!((run_id, seq, block, entries.len()), (RUN_ID, 2, 11, 2));
    }

    #[tokio::test]
    async fn test_publish_and_delta_metrics() {
        let recorder = DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let history = published(&[vec![("a", 1.0)], vec![("a", 2.0), ("b", 3.0)]]);

        history
            .answer(Some(cursor(1)), None)
            .await
            .unwrap();

        let mut recorded = Vec::new();
        for (key, _, _, value) in recorder
            .snapshotter()
            .snapshot()
            .into_vec()
        {
            let value = match value {
                DebugValue::Gauge(block) => block.into_inner(),
                DebugValue::Histogram(values) => values[0].into_inner(),
                DebugValue::Counter(_) => panic!("unexpected counter"),
            };
            recorded.push((key.key().name().to_string(), value));
        }
        recorded.sort_by(|left, right| left.0.cmp(&right.0));
        let names: Vec<_> = recorded
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            names,
            ["nexus_derived_block", "nexus_derived_delta_bytes", "nexus_derived_delta_entries"]
        );
        assert_eq!((recorded[0].1, recorded[2].1), (11.0, 2.0));
        assert!(recorded[1].1 > 0.0);
    }
}
