//! The feed of one RFQ venue's books, polled from a `fynd-nexus` service.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use futures::stream;
use metrics::{counter, gauge};
use reqwest::{
    header::{ETAG, IF_NONE_MATCH},
    StatusCode,
};
use tokio::time::{interval, sleep, Interval, MissedTickBehavior};
use tracing::{debug, info, warn};
use tycho_simulation::{
    book::{BookSnapshot, ReceivedAt},
    snapshot_feed::{errors::FeedError, Publisher, SnapshotFeed},
};

use super::{max_snapshot_age, prices_path, RfqAssembler, RfqVenue};
use crate::nexus::{MessageBody, MessageError, NexusMessage};

/// How often the feed polls the service for current prices.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long one request may take before the poll counts as failed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum retry delay. After failed polls, the delay doubles from [`POLL_INTERVAL`] to this
/// limit, allowing a pod to poll a restarted service well before withdrawing the venue's prices.
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// The cadence and limits of a feed's polling.
#[derive(Debug, Clone, Copy)]
struct PollTimings {
    poll_interval: Duration,
    request_timeout: Duration,
    max_backoff: Duration,
    /// The age at which prices are rejected on arrival and withdrawn when nothing replaces them.
    /// `None` for no limit.
    max_snapshot_age: Option<Duration>,
}

impl PollTimings {
    fn for_venue(venue: RfqVenue) -> Self {
        Self {
            poll_interval: POLL_INTERVAL,
            request_timeout: REQUEST_TIMEOUT,
            max_backoff: MAX_BACKOFF,
            max_snapshot_age: max_snapshot_age(venue),
        }
    }

    /// The wait before the next poll after `failed_polls` polls in a row have failed.
    fn backoff(&self, failed_polls: u32) -> Duration {
        let doublings = failed_polls.saturating_sub(1);
        self.poll_interval
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(self.max_backoff)
    }
}

/// One venue's books, polled from Fynd Nexus and published like a direct feed's.
///
/// It never gives up polling: it withdraws the venue's books once it has applied no new prices for
/// the venue's snapshot age, and restores them when new prices arrive.
pub struct RfqPricesFeed {
    max_snapshot_age: Option<Duration>,
    poller: PricePoller,
}

impl RfqPricesFeed {
    /// A feed of the venue `assembler` reads, from the service at `base_url`, which
    /// must be a valid HTTP URL.
    pub(crate) fn new(base_url: &str, http: reqwest::Client, assembler: RfqAssembler) -> Self {
        let timings = PollTimings::for_venue(assembler.venue());
        Self::with_timings(base_url, http, assembler, timings)
    }

    fn with_timings(
        base_url: &str,
        http: reqwest::Client,
        assembler: RfqAssembler,
        timings: PollTimings,
    ) -> Self {
        let url = format!("{base_url}{}", prices_path(assembler.venue()));
        Self {
            max_snapshot_age: timings.max_snapshot_age,
            poller: PricePoller::new(url, http, assembler, timings),
        }
    }
}

impl SnapshotFeed for RfqPricesFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl std::future::Future<Output = Result<(), FeedError>> + Send + 'static {
        let snapshots = stream::unfold(self.poller, |mut poller| async move {
            let snapshot = poller.next_snapshot().await;
            Some((Ok(snapshot), poller))
        });
        publisher.publishing(self.max_snapshot_age, snapshots)
    }
}

/// The `epoch` and `seq` of a message: which service run sent it and where it sits in the stream.
#[derive(Debug, Clone, Copy)]
struct MessagePosition {
    epoch: u64,
    seq: u64,
}

/// Why a poll failed.
#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("unexpected HTTP response status {0}")]
    Status(StatusCode),
    #[error("{0}")]
    Message(#[from] MessageError),
    #[error("prices do not match the venue's price format: {0}")]
    Prices(serde_json::Error),
}

/// Polls prices for one venue. Converts snapshots into books and withdrawals into
/// empty books. Drops previously applied messages and snapshots older than the venue's snapshot
/// age.
struct PricePoller {
    venue: RfqVenue,
    url: String,
    http: reqwest::Client,
    assembler: RfqAssembler,
    timings: PollTimings,
    ticker: Option<Interval>,
    failed_polls: u32,
    etag: Option<String>,
    last_applied: Option<MessagePosition>,
}

impl PricePoller {
    fn new(
        url: String,
        http: reqwest::Client,
        assembler: RfqAssembler,
        timings: PollTimings,
    ) -> Self {
        Self {
            venue: assembler.venue(),
            url,
            http,
            assembler,
            timings,
            ticker: None,
            failed_polls: 0,
            etag: None,
            last_applied: None,
        }
    }

    /// Polls until books are available to apply, then returns them. Logs and counts failed polls
    /// and retries them after a backoff.
    async fn next_snapshot(&mut self) -> BookSnapshot<ReceivedAt> {
        loop {
            self.wait_for_next_poll().await;
            match self.poll().await {
                Ok(Some(snapshot)) => {
                    self.failed_polls = 0;
                    return snapshot;
                }
                Ok(None) => self.failed_polls = 0,
                Err(error) => self.record_failure(&error),
            }
        }
    }

    async fn wait_for_next_poll(&mut self) {
        let period = self.timings.poll_interval;
        let ticker = self.ticker.get_or_insert_with(|| {
            let mut ticker = interval(period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker
        });
        if self.failed_polls > 0 {
            sleep(self.timings.backoff(self.failed_polls)).await;
        }
        ticker.tick().await;
    }

    fn record_failure(&mut self, error: &PollError) {
        self.failed_polls = self.failed_polls.saturating_add(1);
        counter!("nexus_client_failed_polls_total", "venue" => self.venue.name()).increment(1);
        warn!(
            venue = %self.venue,
            url = %self.url,
            wait = ?self.timings.backoff(self.failed_polls),
            "polling Fynd Nexus failed: {error}"
        );
    }

    /// Asks the service for the current message and applies it. Returns `None` when the service
    /// has nothing new, and the books when it has.
    async fn poll(&mut self) -> Result<Option<BookSnapshot<ReceivedAt>>, PollError> {
        let mut request = self
            .http
            .get(&self.url)
            .timeout(self.timings.request_timeout);
        if let Some(etag) = &self.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_MODIFIED | StatusCode::NO_CONTENT => return Ok(None),
            status => return Err(PollError::Status(status)),
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let text = response.text().await?;
        let message = NexusMessage::<serde_json::Value>::decode(&text)?;
        let snapshot = self.apply_message(message)?;
        self.etag = etag;
        Ok(snapshot)
    }

    fn apply_message(
        &mut self,
        message: NexusMessage<serde_json::Value>,
    ) -> Result<Option<BookSnapshot<ReceivedAt>>, PollError> {
        let NexusMessage { epoch, seq, body, .. } = message;
        let position = MessagePosition { epoch, seq };
        match body {
            MessageBody::Snapshot { as_of, data } => self.apply_snapshot(position, as_of, data),
            MessageBody::Withdrawn => Ok(self.apply_withdrawal(position)),
        }
    }

    fn is_replay(&self, position: MessagePosition) -> bool {
        self.last_applied
            .is_some_and(|last| last.epoch == position.epoch && position.seq <= last.seq)
    }

    /// Records `position` as applied and logs a service restart when the epoch changes.
    fn mark_applied(&mut self, position: MessagePosition) {
        if self
            .last_applied
            .is_some_and(|last| last.epoch != position.epoch)
        {
            info!(venue = %self.venue, epoch = position.epoch, "Fynd Nexus restarted");
        }
        self.last_applied = Some(position);
    }

    fn apply_snapshot(
        &mut self,
        position: MessagePosition,
        as_of: DateTime<Utc>,
        data: serde_json::Value,
    ) -> Result<Option<BookSnapshot<ReceivedAt>>, PollError> {
        let MessagePosition { epoch, seq } = position;
        if self.is_replay(position) {
            debug!(venue = %self.venue, epoch, seq, "dropping a snapshot already applied");
            return Ok(None);
        }
        let age = (Utc::now() - as_of).max(chrono::Duration::zero());
        if self
            .timings
            .max_snapshot_age
            .zip(age.to_std().ok())
            .is_some_and(|(max_age, age)| age > max_age)
        {
            debug!(venue = %self.venue, epoch, seq, %age, "dropping a stale snapshot");
            return Ok(None);
        }
        let books = self
            .assembler
            .books(data)
            .map_err(PollError::Prices)?;
        self.mark_applied(position);
        gauge!("nexus_client_snapshot_age_seconds", "venue" => self.venue.name())
            .set(age.num_milliseconds() as f64 / 1000.0);
        Ok(Some(BookSnapshot { anchor: ReceivedAt(as_of), books: Arc::new(books) }))
    }

    fn apply_withdrawal(&mut self, position: MessagePosition) -> Option<BookSnapshot<ReceivedAt>> {
        if self.is_replay(position) {
            debug!(
                venue = %self.venue,
                epoch = position.epoch,
                seq = position.seq,
                "dropping a withdrawal already applied"
            );
            return None;
        }
        self.mark_applied(position);
        Some(BookSnapshot { anchor: ReceivedAt(Utc::now()), books: Arc::new(HashMap::new()) })
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use rstest::rstest;
    use serde_json::Value;
    use tokio::time::timeout;
    use tycho_simulation::snapshot_feed::{SnapshotFeedEvent, SnapshotFeedStream};
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::nexus::rfq::test_support::{
        bebop_assembler, bebop_prices, hashflow_assembler, hashflow_prices, metric_assembler,
        metric_prices,
    };

    const EPOCH: u64 = 1_761_550_000_000;
    const PATH: &str = "/v1/rfq/bebop";
    const DEADLINE: Duration = Duration::from_secs(10);

    fn timings() -> PollTimings {
        PollTimings {
            poll_interval: Duration::from_millis(5),
            request_timeout: Duration::from_secs(5),
            max_backoff: Duration::from_millis(40),
            max_snapshot_age: Some(Duration::from_secs(60)),
        }
    }

    fn feed_of(url: &str, assembler: RfqAssembler, timings: PollTimings) -> RfqPricesFeed {
        RfqPricesFeed::with_timings(url, reqwest::Client::new(), assembler, timings)
    }

    fn poller_of(url: &str, assembler: RfqAssembler, timings: PollTimings) -> PricePoller {
        feed_of(url, assembler, timings).poller
    }

    fn poller() -> PricePoller {
        poller_of("http://shared:3000", RfqAssembler::Bebop(bebop_assembler()), timings())
    }

    fn snapshot_of(
        data: Value,
        epoch: u64,
        seq: u64,
        age: chrono::Duration,
    ) -> NexusMessage<Value> {
        NexusMessage::new(epoch, seq, MessageBody::Snapshot { as_of: Utc::now() - age, data })
    }

    fn snapshot_with_age(epoch: u64, seq: u64, age: chrono::Duration) -> NexusMessage<Value> {
        snapshot_of(bebop_json(), epoch, seq, age)
    }

    fn snapshot(epoch: u64, seq: u64) -> NexusMessage<Value> {
        snapshot_with_age(epoch, seq, chrono::Duration::milliseconds(200))
    }

    fn withdrawn(epoch: u64, seq: u64) -> NexusMessage<Value> {
        NexusMessage::new(epoch, seq, MessageBody::Withdrawn)
    }

    fn bebop_json() -> Value {
        serde_json::to_value(bebop_prices(3000.5)).unwrap()
    }

    fn hashflow_json() -> Value {
        serde_json::to_value(hashflow_prices()).unwrap()
    }

    fn metric_json() -> Value {
        serde_json::to_value(metric_prices()).unwrap()
    }

    fn etag_of(message: &NexusMessage<Value>) -> String {
        format!("\"{}-{}\"", message.epoch, message.seq)
    }

    fn ok(message: &NexusMessage<Value>) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("ETag", etag_of(message))
            .set_body_string(message.encode().unwrap())
    }

    async fn serve(response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn test_snapshot_book_assembly() {
        let mut poller = poller();
        let as_of = Utc::now() - chrono::Duration::milliseconds(300);
        let data = serde_json::to_value(bebop_prices(3000.1)).unwrap();
        let message =
            NexusMessage::new(EPOCH, 1, MessageBody::Snapshot { as_of, data: data.clone() });

        let applied = poller
            .apply_message(message)
            .unwrap()
            .expect("a fresh snapshot is published");

        let direct = RfqAssembler::Bebop(bebop_assembler())
            .books(data)
            .unwrap();
        let mut applied_ids: Vec<_> = applied.books.keys().collect();
        let mut direct_ids: Vec<_> = direct.keys().collect();
        applied_ids.sort();
        direct_ids.sort();
        assert_eq!(applied.anchor, ReceivedAt(as_of));
        assert!(!direct_ids.is_empty());
        assert_eq!(applied_ids, direct_ids);
    }

    #[test]
    fn test_withdrawal_after_snapshot() {
        let mut poller = poller();
        poller
            .apply_message(snapshot(EPOCH, 1))
            .unwrap();

        let applied = poller
            .apply_message(withdrawn(EPOCH, 2))
            .unwrap()
            .expect("a withdrawal is published");

        assert!(applied.books.is_empty());
    }

    #[rstest]
    #[case::bebop_fresh(RfqAssembler::Bebop(bebop_assembler()), bebop_json(), 5, true)]
    #[case::bebop_just_inside(RfqAssembler::Bebop(bebop_assembler()), bebop_json(), 50, true)]
    #[case::bebop_stale(RfqAssembler::Bebop(bebop_assembler()), bebop_json(), 120, false)]
    #[case::bebop_long_dead(RfqAssembler::Bebop(bebop_assembler()), bebop_json(), 86_400, false)]
    #[case::hashflow_inside(
        RfqAssembler::Hashflow(hashflow_assembler()),
        hashflow_json(),
        20,
        true
    )]
    #[case::hashflow_stale(
        RfqAssembler::Hashflow(hashflow_assembler()),
        hashflow_json(),
        40,
        false
    )]
    #[case::metric_inside(RfqAssembler::Metric(metric_assembler()), metric_json(), 10, true)]
    #[case::metric_stale(RfqAssembler::Metric(metric_assembler()), metric_json(), 20, false)]
    fn test_snapshot_age(
        #[case] assembler: RfqAssembler,
        #[case] data: Value,
        #[case] age_secs: i64,
        #[case] published: bool,
    ) {
        let message = snapshot_of(data, EPOCH, 1, chrono::Duration::seconds(age_secs));
        let venue_timings = PollTimings::for_venue(assembler.venue());

        let applied = poller_of("http://shared:3000", assembler, venue_timings)
            .apply_message(message)
            .unwrap();

        assert_eq!(applied.is_some(), published);
    }

    #[test]
    fn test_snapshot_from_the_future() {
        let message = snapshot_with_age(EPOCH, 1, chrono::Duration::seconds(-30));

        assert!(poller()
            .apply_message(message)
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_message_epoch_and_sequence_ordering() {
        let steps = [
            (snapshot(7, 5), true, "first message"),
            (snapshot(7, 5), false, "same seq again"),
            (snapshot(7, 4), false, "lower seq"),
            (snapshot(7, 6), true, "higher seq"),
            (snapshot(8, 1), true, "new epoch restarts seq"),
            (snapshot(8, 1), false, "replay in the new epoch"),
            (withdrawn(8, 2), true, "withdrawal after snapshot"),
            (snapshot(8, 2), false, "snapshot the withdrawal superseded"),
            (withdrawn(8, 2), false, "repeated withdrawal"),
            (snapshot(8, 3), true, "snapshot after withdrawal"),
        ];
        let mut poller = poller();

        for (message, applied, step) in steps {
            let result = poller.apply_message(message).unwrap();

            assert_eq!(result.is_some(), applied, "{step}");
        }
    }

    #[test]
    fn test_prices_of_another_venue() {
        let mut poller =
            poller_of("http://shared:3000", RfqAssembler::Metric(metric_assembler()), timings());

        let error = poller
            .apply_message(snapshot(EPOCH, 1))
            .unwrap_err();

        assert!(matches!(error, PollError::Prices(_)), "got {error}");
        assert!(poller.last_applied.is_none());
    }

    #[test]
    fn test_snapshot_age_metric() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let mut poller = poller();

        metrics::with_local_recorder(&recorder, || {
            poller
                .apply_message(snapshot_with_age(EPOCH, 1, chrono::Duration::seconds(2)))
                .unwrap();
        });

        let recorded: HashMap<_, _> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, value)| (key.key().name().to_string(), value))
            .collect();
        let DebugValue::Gauge(age) = recorded["nexus_client_snapshot_age_seconds"] else {
            panic!("the snapshot age is a gauge");
        };
        assert!((2.0..3.0).contains(&age.into_inner()), "age was {age}");
    }

    #[rstest]
    #[case::first_failure(1, 500)]
    #[case::second_failure(2, 1_000)]
    #[case::fourth_failure(4, 4_000)]
    #[case::fifth_failure(5, 8_000)]
    #[case::capped(6, 8_000)]
    #[case::many_failures(u32::MAX, 8_000)]
    fn test_backoff_doubles_up_to_the_cap(#[case] failed_polls: u32, #[case] millis: u64) {
        let timings = PollTimings::for_venue(RfqVenue::Bebop);

        assert_eq!(timings.backoff(failed_polls), Duration::from_millis(millis));
    }

    #[tokio::test]
    async fn test_poll_applies_a_snapshot_then_sends_its_etag() {
        let first = snapshot(EPOCH, 1);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header("if-none-match", etag_of(&first).as_str()))
            .respond_with(ResponseTemplate::new(304))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(ok(&first))
            .mount(&server)
            .await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());

        let applied = poller.poll().await.unwrap();
        let unchanged = poller.poll().await.unwrap();

        assert!(!applied
            .expect("the snapshot from the HTTP 200 response is applied")
            .books
            .is_empty());
        assert!(unchanged.is_none());
        let requests = server
            .received_requests()
            .await
            .unwrap();
        let sent: Vec<_> = requests
            .iter()
            .map(|request| {
                request
                    .headers
                    .get("if-none-match")
                    .map(|v| v.to_str().unwrap())
            })
            .collect();
        assert_eq!(sent, [None, Some("\"1761550000000-1\"")]);
    }

    #[rstest]
    #[case::not_modified(304)]
    #[case::no_content(204)]
    #[tokio::test]
    async fn test_poll_without_updates_with_existing_etag(#[case] status: u16) {
        let server = serve(ResponseTemplate::new(status)).await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());
        poller.etag = Some("\"7-3\"".to_string());

        let result = poller.poll().await.unwrap();

        assert!(result.is_none());
        assert_eq!(poller.etag.as_deref(), Some("\"7-3\""));
    }

    #[tokio::test]
    async fn test_poll_withdrawal() {
        let server = serve(ok(&withdrawn(EPOCH, 4))).await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());

        let applied = poller.poll().await.unwrap();

        assert!(applied
            .expect("a withdrawal is published")
            .books
            .is_empty());
    }

    #[tokio::test]
    async fn test_poll_skips_a_replay_but_remembers_its_etag() {
        let server = serve(ok(&snapshot(EPOCH, 2))).await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());
        poller
            .apply_message(snapshot(EPOCH, 2))
            .unwrap();

        let result = poller.poll().await.unwrap();

        assert!(result.is_none());
        assert_eq!(poller.etag.as_deref(), Some("\"1761550000000-2\""));
    }

    #[tokio::test]
    async fn test_poll_skips_a_stale_snapshot() {
        let stale = snapshot_with_age(EPOCH, 1, chrono::Duration::seconds(120));
        let server = serve(ok(&stale)).await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());

        assert!(poller.poll().await.unwrap().is_none());
    }

    #[rstest]
    #[case::not_found(ResponseTemplate::new(404), "404")]
    #[case::server_error(ResponseTemplate::new(500), "500")]
    #[case::garbage(ResponseTemplate::new(200).set_body_string("not json"), "malformed")]
    #[case::other_version(
        ResponseTemplate::new(200).set_body_string(r#"{"v":2,"type":"withdrawn"}"#),
        "version 2"
    )]
    #[case::other_venues_prices(ok(&snapshot_of(metric_json(), EPOCH, 1, chrono::Duration::zero())),
        "price format")]
    #[tokio::test]
    async fn test_poll_response_errors(#[case] response: ResponseTemplate, #[case] reason: &str) {
        let server = serve(response).await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());

        let error = poller.poll().await.unwrap_err();

        assert!(error.to_string().contains(reason), "got {error}");
        assert!(poller.etag.is_none());
        assert!(poller.last_applied.is_none());
    }

    #[tokio::test]
    async fn test_poll_timeout() {
        let server = serve(ok(&snapshot(EPOCH, 1)).set_delay(Duration::from_secs(30))).await;
        let slow = PollTimings { request_timeout: Duration::from_millis(50), ..timings() };
        let mut poller = poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), slow);

        let error = poller.poll().await.unwrap_err();

        assert!(matches!(error, PollError::Request(_)), "got {error}");
    }

    #[tokio::test]
    async fn test_poll_unreachable_service() {
        let mut poller =
            poller_of("http://127.0.0.1:1", RfqAssembler::Bebop(bebop_assembler()), timings());

        let error = poller.poll().await.unwrap_err();

        assert!(matches!(error, PollError::Request(_)), "got {error}");
    }

    #[tokio::test]
    async fn test_next_snapshot_keeps_polling_after_failures_and_counts_them() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(3)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(2)
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(ok(&snapshot(EPOCH, 1)))
            .with_priority(3)
            .mount(&server)
            .await;
        let mut poller =
            poller_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), timings());

        let books = timeout(DEADLINE, poller.next_snapshot())
            .await
            .expect("the poller recovers once the service answers");

        assert!(!books.books.is_empty());
        assert_eq!(poller.failed_polls, 0, "a success resets the backoff");
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .len(),
            6
        );
        let failed_polls = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(key, _, _, value)| match value {
                DebugValue::Counter(count)
                    if key.key().name() == "nexus_client_failed_polls_total" =>
                {
                    Some(count)
                }
                _ => None,
            });
        assert_eq!(failed_polls, Some(3));
    }

    #[tokio::test]
    async fn test_feed_withdraws_prices_that_stop_changing() {
        let first = snapshot(EPOCH, 1);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(header("if-none-match", etag_of(&first).as_str()))
            .respond_with(ResponseTemplate::new(304))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(|_: &wiremock::Request| {
                ok(&snapshot_with_age(EPOCH, 1, chrono::Duration::zero()))
            })
            .mount(&server)
            .await;
        let aging = PollTimings { max_snapshot_age: Some(Duration::from_millis(150)), ..timings() };
        let feed = feed_of(&server.uri(), RfqAssembler::Bebop(bebop_assembler()), aging);
        let mut events = SnapshotFeedStream::spawn("test", feed);

        let published = timeout(DEADLINE, events.next())
            .await
            .unwrap();
        let withdrawn = timeout(DEADLINE, events.next())
            .await
            .unwrap();

        assert!(matches!(published, Some(SnapshotFeedEvent::Published(_))));
        assert!(matches!(withdrawn, Some(SnapshotFeedEvent::Withdrawn)));
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .len() >
                2,
            "the feed should make more than two polls"
        );
    }
}
