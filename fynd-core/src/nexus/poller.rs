//! The polling loop that every stream of a `fynd-nexus` service shares.

use std::time::Duration;

use chrono::{DateTime, Utc};
use metrics::{counter, gauge};
use reqwest::{
    header::{ETAG, IF_NONE_MATCH},
    StatusCode,
};
use tokio::time::{interval, sleep, Interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::{MessageBody, MessageError, NexusMessage};

/// How often a stream is polled for its current message.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long one request may take before the poll counts as failed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum retry delay. After failed polls, the delay doubles from [`POLL_INTERVAL`] to this
/// limit, allowing a pod to poll a restarted service well before withdrawing what it applied.
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// The cadence and limits of a stream's polling.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PollTimings {
    pub(crate) poll_interval: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) max_backoff: Duration,
    /// The age at which a snapshot is rejected on arrival. `None` for no limit.
    pub(crate) max_snapshot_age: Option<Duration>,
}

impl PollTimings {
    /// The standard cadence, rejecting snapshots older than `max_snapshot_age`.
    pub(crate) fn new(max_snapshot_age: Option<Duration>) -> Self {
        Self {
            poll_interval: POLL_INTERVAL,
            request_timeout: REQUEST_TIMEOUT,
            max_backoff: MAX_BACKOFF,
            max_snapshot_age,
        }
    }

    /// The wait before the next poll after `failed_polls` polls in a row have failed.
    pub(crate) fn backoff(&self, failed_polls: u32) -> Duration {
        let doublings = failed_polls.saturating_sub(1);
        self.poll_interval
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(self.max_backoff)
    }
}

/// Turns the messages of one stream into what the pod applies.
pub(crate) trait StreamApplier {
    /// What the pod applies: the stream's state, or its absence.
    type Update;

    /// The name of the stream. It is the `venue` label of the poll metrics and part of the logs.
    fn stream(&self) -> &'static str;

    /// Reads the data of a snapshot that the service got at `as_of`.
    ///
    /// # Errors
    ///
    /// The JSON error when `data` is not this stream's format.
    fn snapshot(
        &mut self,
        data: serde_json::Value,
        as_of: DateTime<Utc>,
    ) -> Result<Self::Update, serde_json::Error>;

    /// What the pod applies when the service has no usable state.
    fn withdrawal(&mut self) -> Self::Update;
}

/// The `epoch` and `seq` of a message: which service run sent it and where it sits in the stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MessagePosition {
    epoch: u64,
    seq: u64,
}

/// Why a poll failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PollError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("unexpected HTTP response status {0}")]
    Status(StatusCode),
    #[error("{0}")]
    Message(#[from] MessageError),
    #[error("data does not match the stream's format: {0}")]
    Data(serde_json::Error),
}

/// What one poll of a stream came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Polled<U> {
    /// The service answered with a message to apply.
    Update(U),
    /// The service answered, with nothing new to apply: its message is the one already applied,
    /// or a replay or a stale snapshot of it.
    Unchanged,
    /// The service answered that it has no state for the stream, such as after a restart.
    Empty,
    /// The poll failed. It was logged and counted.
    Failed,
}

impl<U> Polled<U> {
    /// The update, if the poll came back with one.
    #[cfg(test)]
    pub(crate) fn update(self) -> Option<U> {
        match self {
            Self::Update(update) => Some(update),
            Self::Unchanged | Self::Empty | Self::Failed => None,
        }
    }
}

/// Polls one stream of a `fynd-nexus` service. Hands each new message to the applier, and drops
/// messages it has applied before and snapshots older than the snapshot age limit.
pub(crate) struct StreamPoller<A: StreamApplier> {
    url: String,
    http: reqwest::Client,
    applier: A,
    timings: PollTimings,
    ticker: Option<Interval>,
    pub(crate) failed_polls: u32,
    pub(crate) etag: Option<String>,
    pub(crate) last_applied: Option<MessagePosition>,
}

impl<A: StreamApplier> StreamPoller<A> {
    /// A poller of the stream served at `url`.
    pub(crate) fn new(
        url: String,
        http: reqwest::Client,
        applier: A,
        timings: PollTimings,
    ) -> Self {
        Self {
            url,
            http,
            applier,
            timings,
            ticker: None,
            failed_polls: 0,
            etag: None,
            last_applied: None,
        }
    }

    /// Polls until there is an update to apply, then returns it. Logs and counts failed polls
    /// and retries them after a backoff.
    pub(crate) async fn next_update(&mut self) -> A::Update {
        loop {
            if let Polled::Update(update) = self.poll_once().await {
                return update;
            }
        }
    }

    /// Waits for the next poll and makes it. A failed poll is logged and counted, and the next
    /// one waits for a backoff.
    pub(crate) async fn poll_once(&mut self) -> Polled<A::Update> {
        self.wait_for_next_poll().await;
        match self.poll().await {
            Ok(polled) => {
                self.failed_polls = 0;
                polled
            }
            Err(error) => {
                self.record_failure(&error);
                Polled::Failed
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
        counter!("nexus_client_failed_polls_total", "venue" => self.applier.stream()).increment(1);
        warn!(
            stream = self.applier.stream(),
            url = %self.url,
            wait = ?self.timings.backoff(self.failed_polls),
            "polling Fynd Nexus failed: {error}"
        );
    }

    /// Asks the service for the current message and applies it.
    pub(crate) async fn poll(&mut self) -> Result<Polled<A::Update>, PollError> {
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
            StatusCode::NOT_MODIFIED => return Ok(Polled::Unchanged),
            StatusCode::NO_CONTENT => return Ok(Polled::Empty),
            status => return Err(PollError::Status(status)),
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let text = response.text().await?;
        let message = NexusMessage::<serde_json::Value>::decode(&text)?;
        let update = self.apply_message(message)?;
        self.etag = etag;
        Ok(update.map_or(Polled::Unchanged, Polled::Update))
    }

    pub(crate) fn apply_message(
        &mut self,
        message: NexusMessage<serde_json::Value>,
    ) -> Result<Option<A::Update>, PollError> {
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
            info!(stream = self.applier.stream(), epoch = position.epoch, "Fynd Nexus restarted");
        }
        self.last_applied = Some(position);
    }

    fn apply_snapshot(
        &mut self,
        position: MessagePosition,
        as_of: DateTime<Utc>,
        data: serde_json::Value,
    ) -> Result<Option<A::Update>, PollError> {
        let MessagePosition { epoch, seq } = position;
        let stream = self.applier.stream();
        if self.is_replay(position) {
            debug!(stream, epoch, seq, "dropping a snapshot already applied");
            return Ok(None);
        }
        let age = (Utc::now() - as_of).max(chrono::Duration::zero());
        if self
            .timings
            .max_snapshot_age
            .zip(age.to_std().ok())
            .is_some_and(|(max_age, age)| age > max_age)
        {
            debug!(stream, epoch, seq, %age, "dropping a stale snapshot");
            return Ok(None);
        }
        let update = self
            .applier
            .snapshot(data, as_of)
            .map_err(PollError::Data)?;
        self.mark_applied(position);
        gauge!("nexus_client_snapshot_age_seconds", "venue" => stream)
            .set(age.num_milliseconds() as f64 / 1000.0);
        Ok(Some(update))
    }

    fn apply_withdrawal(&mut self, position: MessagePosition) -> Option<A::Update> {
        if self.is_replay(position) {
            debug!(
                stream = self.applier.stream(),
                epoch = position.epoch,
                seq = position.seq,
                "dropping a withdrawal already applied"
            );
            return None;
        }
        self.mark_applied(position);
        Some(self.applier.withdrawal())
    }
}
