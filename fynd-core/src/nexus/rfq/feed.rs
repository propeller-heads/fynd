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
    pub(crate) fn new(
        base_url: &str,
        http: reqwest::Client,
        assembler: RfqAssembler,
    ) -> Self {
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
