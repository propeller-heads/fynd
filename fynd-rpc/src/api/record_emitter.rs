//! The bounded queue between the quote handler and the task that sends records to the collector.
//!
//! The handler hands every finished [`QuoteRecord`](crate::api::record::QuoteRecord) to
//! [`RecordEmitter::emit`], which neither blocks nor fails: a record the queue has no room for is
//! dropped and counted. Dropping the incoming record rather than evicting an older one leaves the
//! store a clean prefix of the traffic while the collector is out, instead of a sample with gaps.

use metrics::counter;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::api::record::QuoteRecord;

/// The quote handler's end of the record queue.
#[derive(Clone, Debug)]
pub struct RecordEmitter {
    sender: mpsc::Sender<QuoteRecord>,
}

impl RecordEmitter {
    /// Creates the queue, returning the emitter the handler holds and the receiver the sending
    /// task drains. The queue holds `capacity` records; one arriving at a full queue is dropped.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    #[must_use]
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<QuoteRecord>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender }, receiver)
    }

    /// Queues `record`, or drops it and counts the drop. Called on the response path, so it only
    /// ever moves the record into the queue — nothing it does waits on the sending task.
    pub fn emit(&self, record: QuoteRecord) {
        match self.sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => record_drop("queue_full"),
            Err(TrySendError::Closed(_)) => record_drop("sink_closed"),
        }
    }
}

/// Counts one record this pod will not store, under `quote_records_dropped_total{reason}`.
///
/// The reasons: `queue_full` when the queue had no room for it, `sink_closed` when the sending
/// task is gone, and `sink_timeout` / `sink_rejected` when that task could not hand it to the
/// collector.
pub(crate) fn record_drop(reason: &'static str) {
    counter!("quote_records_dropped_total", "reason" => reason).increment(1);
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use actix_web::http::header::HeaderMap;
    use fynd_core::{ExclusiveAccess, QuoteRequest, SolveError};
    use fynd_rpc_types::{Bytes, Order, OrderSide};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use num_bigint::BigUint;
    use tycho_simulation::tycho_common::models::Chain;

    use super::*;
    use crate::api::{middleware::ClientInfo, record::RequestRecord};

    /// A record carrying `amount` as its single order's input, so a test can tell records apart.
    fn record(amount: u64) -> QuoteRecord {
        let order = Order::new(
            Bytes::from([0xAAu8; 20]),
            Bytes::from([0xBBu8; 20]),
            BigUint::from(amount),
            OrderSide::Sell,
            Bytes::from([0xCCu8; 20]),
        );
        let request: QuoteRequest = fynd_rpc_types::QuoteRequest::new(vec![order]).into();
        QuoteRecord::build(
            RequestRecord::capture(&request, ExclusiveAccess::Denied),
            &Err(SolveError::QueueFull),
            Chain::Ethereum,
            ClientInfo::from_headers(&HeaderMap::new()),
            None,
        )
    }

    /// The amount the record's single order carries, as `record` set it.
    fn amount_of(record: &QuoteRecord) -> String {
        let value = serde_json::to_value(record).unwrap();
        value["request"]["orders"][0]["amount"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Runs `f` against a local recorder and returns the drop counts by reason.
    fn drops_by_reason(f: impl FnOnce()) -> Vec<(String, u64)> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, f);
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, _, _, _)| key.key().name() == "quote_records_dropped_total")
            .map(|(key, _, _, value)| {
                let reason = key
                    .key()
                    .labels()
                    .find(|label| label.key() == "reason")
                    .expect("drop carries a reason")
                    .value()
                    .to_string();
                let DebugValue::Counter(count) = value else { panic!("not a counter: {value:?}") };
                (reason, count)
            })
            .collect()
    }

    #[test]
    fn test_emit_queues_record() {
        let (emitter, mut receiver) = RecordEmitter::new(4);

        let drops = drops_by_reason(|| emitter.emit(record(1)));

        assert!(drops.is_empty(), "nothing was dropped: {drops:?}");
        assert_eq!(
            amount_of(
                &receiver
                    .try_recv()
                    .expect("record queued")
            ),
            "1"
        );
    }

    /// A full queue drops what arrives and keeps what it holds, so the store gets a clean prefix.
    #[test]
    fn test_emit_drops_incoming_record_when_queue_is_full() {
        let (emitter, mut receiver) = RecordEmitter::new(1);
        emitter.emit(record(1));

        let drops = drops_by_reason(|| emitter.emit(record(2)));

        assert_eq!(drops, vec![("queue_full".to_string(), 1)]);
        assert_eq!(
            amount_of(
                &receiver
                    .try_recv()
                    .expect("first record kept")
            ),
            "1"
        );
        assert!(receiver.try_recv().is_err(), "the dropped record was not queued");
    }

    #[test]
    fn test_emit_counts_drops_once_the_sending_task_is_gone() {
        let (emitter, receiver) = RecordEmitter::new(1);
        drop(receiver);

        let drops = drops_by_reason(|| emitter.emit(record(1)));

        assert_eq!(drops, vec![("sink_closed".to_string(), 1)]);
    }

    /// Emitting into a full queue neither panics nor waits: the response path never pays for a
    /// collector outage. The bound is loose — a blocking send would not return at all.
    #[test]
    fn test_emit_into_full_queue_returns_immediately() {
        let (emitter, _receiver) = RecordEmitter::new(1);
        emitter.emit(record(1));
        let records: Vec<QuoteRecord> = (0..1_000).map(record).collect();

        let started = Instant::now();
        for record in records {
            emitter.emit(record);
        }

        assert!(started.elapsed() < Duration::from_millis(100), "took {:?}", started.elapsed());
    }
}
