//! The bounded queue between the quote handler and the task that sends records to the collector.
//!
//! The handler hands every finished [`QuoteRecord`](crate::api::record::QuoteRecord) to
//! [`RecordEmitter::emit`], which neither blocks nor fails: a record the queue has no room for is
//! dropped and counted. Dropping the incoming record rather than evicting an older one leaves the
//! store a clean prefix of the traffic while the collector is out, instead of a sample with gaps.
//!
//! [`record_sink`] builds the other end: a task that drains the queue, batches what it finds and
//! POSTs each batch to `<record_sink_url>/v1/records`, zstd-compressed, at least once a second. A
//! batch that times out or is refused is dropped and counted, never retried — the collector mints
//! a record id per record on receipt, so a second attempt at a batch that did arrive stores every
//! record in it twice, and nothing downstream can tell the copies apart.

use std::num::NonZeroUsize;

use anyhow::{Context, Result};
use metrics::counter;
use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE};
use tokio::{
    sync::mpsc::{self, error::TrySendError},
    task::JoinHandle,
    time::{timeout_at, Instant},
};
use tracing::{error, info, warn};

use crate::{api::record::QuoteRecord, config::defaults};

/// The quote handler's end of the record queue.
#[derive(Clone, Debug)]
pub(crate) struct RecordEmitter {
    sender: mpsc::Sender<QuoteRecord>,
}

impl RecordEmitter {
    /// Creates the queue, returning the emitter the handler holds and the receiver the sending
    /// task drains. The queue holds `capacity` records; one arriving at a full queue is dropped.
    ///
    /// The capacity is a [`NonZeroUsize`] because a queue of zero has no room for anything: it
    /// would drop every record, so a misread config would silence the whole feed rather than fail
    /// where it was read.
    #[must_use]
    pub(crate) fn new(capacity: NonZeroUsize) -> (Self, mpsc::Receiver<QuoteRecord>) {
        let (sender, receiver) = mpsc::channel(capacity.get());
        (Self { sender }, receiver)
    }

    /// Queues `record`, or drops it and counts the drop. Called on the response path, so it only
    /// ever moves the record into the queue — nothing it does waits on the sending task.
    pub(crate) fn emit(&self, record: QuoteRecord) {
        match self.sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => record_drop("queue_full", 1),
            Err(TrySendError::Closed(_)) => record_drop("sink_closed", 1),
        }
    }
}

/// Counts `records` this pod will not store, under `quote_records_dropped_total{reason}`.
///
/// The reasons: `queue_full` when the queue had no room for them, `sink_closed` when the sending
/// task is gone, `sink_timeout` when the POST carrying them did not finish in time, and
/// `sink_rejected` when the collector refused them or could not be reached.
fn record_drop(reason: &'static str, records: u64) {
    counter!("quote_records_dropped_total", "reason" => reason).increment(records);
}

/// Builds the record queue and starts the task draining it into the collector at `sink_url`.
///
/// Returns `Ok(None)` when no collector is configured: no queue, no task, and a handler holding
/// no emitter records nothing. `sink_url` is the collector's root — the task appends the
/// `/v1/records` path itself.
///
/// # Errors
///
/// Returns an error when `sink_url` is not a URL, so a typo stops the pod at startup rather than
/// silently dropping every record it serves.
///
/// # Panics
///
/// Spawns with [`tokio::spawn`], which panics when called outside a runtime.
pub(crate) fn record_sink(
    sink_url: Option<&str>,
) -> Result<Option<(RecordEmitter, JoinHandle<()>)>> {
    let Some(sink_url) = sink_url else {
        return Ok(None);
    };
    let sink = RecordSink::new(sink_url)?;
    info!(url = %sink.records_url, "emitting quote records");
    let (emitter, receiver) = RecordEmitter::new(defaults::RECORD_QUEUE_CAPACITY);
    Ok(Some((emitter, tokio::spawn(drain_into(sink, receiver)))))
}

/// Drains `receiver` into `sink`, one batch at a time, until the queue closes.
///
/// A batch is sent when it fills up or when [`RECORD_FLUSH_INTERVAL`](defaults::
/// RECORD_FLUSH_INTERVAL) has passed since its first record, whichever comes first, so a quiet
/// pod still ships what it has every second. Sending is sequential: while a POST is in flight the
/// queue absorbs what the handler serves, and drops it once full.
async fn drain_into(sink: RecordSink, mut receiver: mpsc::Receiver<QuoteRecord>) {
    while let Some(first) = receiver.recv().await {
        let deadline = Instant::now() + defaults::RECORD_FLUSH_INTERVAL;
        let mut batch = Batch::default();
        batch.push(&first);
        while !batch.is_full() {
            match timeout_at(deadline, receiver.recv()).await {
                Ok(Some(record)) => batch.push(&record),
                // Every emitter is gone: ship what this batch holds, then stop.
                Ok(None) => {
                    sink.post(batch).await;
                    return;
                }
                Err(_elapsed) => break,
            }
        }
        sink.post(batch).await;
    }
}

/// One POST's worth of records, serialized as they arrive so the byte cap is exact.
#[derive(Default)]
struct Batch {
    records: Vec<String>,
    bytes: usize,
}

impl Batch {
    /// Serializes `record` into the batch, or counts it as dropped when it cannot be serialized.
    fn push(&mut self, record: &QuoteRecord) {
        match serde_json::to_string(record) {
            Ok(json) => {
                self.bytes += json.len() + 1; // The comma or bracket that follows it.
                self.records.push(json);
            }
            // Every field of a record is an owned primitive, so this needs a bug in the record
            // types. The collector would refuse the record either way.
            Err(error) => {
                error!(%error, "dropping a quote record that cannot be serialized");
                record_drop("sink_rejected", 1);
            }
        }
    }

    /// Whether the batch has reached either cap. Checked after a push, so the record that crossed
    /// the byte cap travels with this batch rather than being dropped for arriving last.
    fn is_full(&self) -> bool {
        self.records.len() >= defaults::RECORD_BATCH_MAX_RECORDS ||
            self.bytes >= defaults::RECORD_BATCH_MAX_BYTES
    }

    fn len(&self) -> usize {
        self.records.len()
    }

    /// The request body the collector reads: `{"records": [...]}`, its `RecordBatch`.
    fn into_body(self) -> String {
        format!(r#"{{"records":[{}]}}"#, self.records.join(","))
    }
}

/// The collector endpoint, and the client that posts to it.
struct RecordSink {
    client: reqwest::Client,
    records_url: String,
}

impl RecordSink {
    /// Builds the sink for the collector rooted at `sink_url`, failing on a URL that is not one.
    fn new(sink_url: &str) -> Result<Self> {
        let records_url = format!("{}/v1/records", sink_url.trim_end_matches('/'));
        let parsed = reqwest::Url::parse(&records_url)
            .with_context(|| format!("record sink URL is not a URL: {sink_url}"))?;
        // `Url::parse` accepts `collector:8080`, reading the host as the scheme.
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "record sink URL must be http or https: {sink_url}"
        );
        Ok(Self { client: reqwest::Client::new(), records_url })
    }

    /// Posts one batch, counting every record in it as dropped when it does not arrive.
    async fn post(&self, batch: Batch) {
        let records = batch.len() as u64;
        if records == 0 {
            return;
        }
        let body = match zstd::encode_all(batch.into_body().as_bytes(), COMPRESSION_LEVEL) {
            Ok(body) => body,
            Err(error) => {
                error!(%error, records, "dropping a batch that could not be compressed");
                record_drop("sink_rejected", records);
                return;
            }
        };
        let response = self
            .client
            .post(&self.records_url)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_ENCODING, "zstd")
            .timeout(defaults::RECORD_SINK_TIMEOUT)
            .body(body)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                warn!(status = %response.status(), records, "collector refused a batch of records");
                record_drop("sink_rejected", records);
            }
            Err(error) if error.is_timeout() => {
                warn!(%error, records, "collector did not answer in time");
                record_drop("sink_timeout", records);
            }
            Err(error) => {
                warn!(%error, records, "could not reach the collector");
                record_drop("sink_rejected", records);
            }
        }
    }
}

/// zstd's own default. The batch is compressed off the response path, but spending more CPU on a
/// body already far under the collector's limit buys nothing.
const COMPRESSION_LEVEL: i32 = 3;

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use actix_web::http::header::HeaderMap;
    use fynd_core::{ExclusiveAccess, QuoteRequest, SolveError};
    use fynd_rpc_types::{Bytes, Order, OrderSide};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use num_bigint::BigUint;
    use serde_json::Value;
    use tycho_simulation::tycho_common::models::Chain;

    use super::*;
    use crate::api::{middleware::ClientInfo, record::RequestRecord};

    /// A queue capacity, as the tests write it.
    fn queue_of(capacity: usize) -> NonZeroUsize {
        NonZeroUsize::new(capacity).expect("a test never asks for an empty queue")
    }

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
        drops_of(snapshotter.snapshot().into_vec())
    }

    /// The `quote_records_dropped_total` counts in a recorder snapshot, by reason.
    fn drops_of(
        recorded: Vec<(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )>,
    ) -> Vec<(String, u64)> {
        recorded
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
        let (emitter, mut receiver) = RecordEmitter::new(queue_of(4));

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
        let (emitter, mut receiver) = RecordEmitter::new(queue_of(1));
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
        let (emitter, receiver) = RecordEmitter::new(queue_of(1));
        drop(receiver);

        let drops = drops_by_reason(|| emitter.emit(record(1)));

        assert_eq!(drops, vec![("sink_closed".to_string(), 1)]);
    }

    /// Emitting into a full queue neither panics nor waits: the response path never pays for a
    /// collector outage. The bound is loose — a blocking send would not return at all.
    #[test]
    fn test_emit_into_full_queue_returns_immediately() {
        let (emitter, _receiver) = RecordEmitter::new(queue_of(1));
        emitter.emit(record(1));
        let records: Vec<QuoteRecord> = (0..1_000).map(record).collect();

        let started = Instant::now();
        for record in records {
            emitter.emit(record);
        }

        assert!(started.elapsed() < Duration::from_millis(100), "took {:?}", started.elapsed());
    }

    /// A stub collector answering `POST /v1/records` with `status`, after `delay`.
    async fn stub_collector(status: u16, delay: Duration) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/records"))
            .respond_with(wiremock::ResponseTemplate::new(status).set_delay(delay))
            .mount(&server)
            .await;
        server
    }

    /// The batch a stub collector received, decompressed and parsed.
    async fn received_batch(server: &wiremock::MockServer) -> Value {
        let requests = server
            .received_requests()
            .await
            .expect("the stub records requests");
        assert_eq!(requests.len(), 1, "expected exactly one POST, got {}", requests.len());
        let request = &requests[0];
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.headers["content-encoding"], "zstd");
        let body = zstd::decode_all(request.body.as_slice()).expect("body is zstd");
        serde_json::from_slice(&body).expect("body is the collector's JSON")
    }

    /// Feeds `records` through a sink pointed at `server` and returns the drops it counted. The
    /// emitter is dropped straight away, so the task ships one batch and stops instead of
    /// waiting out the flush interval.
    async fn drain_to(server: &wiremock::MockServer, records: u64) -> Vec<(String, u64)> {
        let sink = RecordSink::new(&server.uri()).expect("the stub's URI is a URL");
        let (emitter, receiver) = RecordEmitter::new(queue_of(16));
        for amount in 1..=records {
            emitter.emit(record(amount));
        }
        drop(emitter);
        drops_by_reason_async(drain_into(sink, receiver)).await
    }

    /// [`drops_by_reason`] for a future: the local recorder is thread-local, and these tests run
    /// on a current-thread runtime, so the future is driven to completion inside the closure.
    async fn drops_by_reason_async(
        task: impl std::future::Future<Output = ()>,
    ) -> Vec<(String, u64)> {
        let mut task = Box::pin(task);
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        std::future::poll_fn(|cx| {
            metrics::with_local_recorder(&recorder, || task.as_mut().poll(cx))
        })
        .await;
        drops_of(snapshotter.snapshot().into_vec())
    }

    /// Every record queued before the sink shuts down arrives, in one batch.
    #[tokio::test]
    async fn test_sink_posts_queued_records_in_one_batch() {
        let server = stub_collector(202, Duration::ZERO).await;

        let drops = drain_to(&server, 3).await;

        assert!(drops.is_empty(), "nothing was dropped: {drops:?}");
        let batch = received_batch(&server).await;
        let records = batch["records"]
            .as_array()
            .expect("the body carries a records array");
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["request"]["orders"][0]["amount"], "1");
        assert_eq!(records[2]["request"]["orders"][0]["amount"], "3");
    }

    /// A collector that does not answer in time costs the batch, counted apart from a refusal.
    #[tokio::test]
    async fn test_sink_counts_a_timeout() {
        let server =
            stub_collector(202, defaults::RECORD_SINK_TIMEOUT + Duration::from_secs(1)).await;

        let drops = drain_to(&server, 2).await;

        assert_eq!(drops, vec![("sink_timeout".to_string(), 2)]);
    }

    /// A refused batch is dropped where it stands. Retrying it would duplicate every record in
    /// it, since the collector mints the ids.
    #[tokio::test]
    async fn test_sink_counts_a_refusal() {
        let server = stub_collector(400, Duration::ZERO).await;

        let drops = drain_to(&server, 2).await;

        assert_eq!(drops, vec![("sink_rejected".to_string(), 2)]);
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("the stub records requests")
                .len(),
            1,
            "a refused batch is never retried"
        );
    }

    /// No collector configured, nothing built: no queue to fill and no task to run.
    #[tokio::test]
    async fn test_no_sink_without_a_url() {
        assert!(record_sink(None)
            .expect("no URL is not an error")
            .is_none());
    }

    #[tokio::test]
    async fn test_sink_rejects_a_url_that_is_not_one() {
        assert!(record_sink(Some("collector.internal:8080")).is_err());
    }

    /// The path is appended to the collector's root, whether or not it ends in a slash.
    #[rstest::rstest]
    #[case("http://collector.internal:8080")]
    #[case("http://collector.internal:8080/")]
    fn test_sink_url_names_the_records_endpoint(#[case] root: &str) {
        let sink = RecordSink::new(root).expect("a URL");
        assert_eq!(sink.records_url, "http://collector.internal:8080/v1/records");
    }

    #[test]
    fn test_batch_fills_at_the_record_cap() {
        let mut batch = Batch::default();
        let record = record(1);
        for _ in 0..defaults::RECORD_BATCH_MAX_RECORDS - 1 {
            batch.push(&record);
        }

        assert!(!batch.is_full(), "a batch under the cap has room");
        batch.push(&record);
        assert!(batch.is_full());
    }

    /// The body is the collector's `RecordBatch`, not a bare array.
    #[test]
    fn test_batch_body_wraps_the_records() {
        let mut batch = Batch::default();
        batch.push(&record(1));
        batch.push(&record(2));

        let body: Value = serde_json::from_str(&batch.into_body()).expect("valid JSON");

        assert_eq!(
            body["records"]
                .as_array()
                .expect("a records array")
                .len(),
            2
        );
    }
}
