//! The latest message of each venue's stream, kept in memory for the API to serve.

use std::{
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard},
    time::Duration,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use fynd_core::nexus::{rfq::RfqVenue, MessageBody, NexusMessage};
use metrics::{gauge, histogram, Label};
use serde::Serialize;
use tycho_simulation::evm::tycho_models::Chain;

/// How often the snapshot age gauges are refreshed.
const AGE_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// What a stream holds in the current service run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// No snapshot has arrived in this run yet. Pods keep the prices they hold.
    AwaitingSnapshot {
        /// When the run started.
        started_at: DateTime<Utc>,
    },
    /// The stream holds prices.
    Serving {
        /// When the service received the prices.
        as_of: DateTime<Utc>,
    },
    /// The prices went stale or the feed ended. Pods drop the prices they hold.
    Withdrawn {
        /// When the service received the last prices, or the service start time if no prices
        /// arrived.
        last_as_of: DateTime<Utc>,
    },
}

impl StreamState {
    /// The time of the newest prices the stream has held, or the start of the run if it has held
    /// none.
    fn newest_as_of(self) -> DateTime<Utc> {
        match self {
            Self::AwaitingSnapshot { started_at: time } |
            Self::Serving { as_of: time } |
            Self::Withdrawn { last_as_of: time } => time,
        }
    }
}

/// The message a stream serves: a snapshot or a withdrawal, encoded once.
#[derive(Debug, Clone)]
pub struct LatestMessage {
    /// Identifies the service run that made the message.
    pub epoch: u64,
    /// The position of the message in its stream.
    pub seq: u64,
    /// The message as JSON. Clones share one buffer.
    pub encoded: Bytes,
}

impl LatestMessage {
    /// The HTTP `ETag` of this message, such as `"1761550000000-42"`. It changes whenever the
    /// stream's message does, including across service restarts.
    pub fn etag(&self) -> String {
        format!("\"{}-{}\"", self.epoch, self.seq)
    }
}

/// A stream's sequence number, encoded message, and state, guarded by one lock in
/// [`LatestPrices`].
struct StreamRecord {
    seq: u64,
    encoded: Option<Bytes>,
    state: StreamState,
}

/// The latest message of one stream and the state of the stream.
///
/// Only the stream's feed runner publishes or withdraws.
pub struct LatestPrices {
    chain: Chain,
    venue: RfqVenue,
    epoch: u64,
    latest: RwLock<StreamRecord>,
}

impl LatestPrices {
    /// The stream of `venue` on the chain the service serves, with no snapshot yet. `epoch`
    /// identifies this run of the service.
    pub fn new(chain: Chain, venue: RfqVenue, epoch: u64) -> Self {
        let latest = StreamRecord {
            seq: 0,
            encoded: None,
            state: StreamState::AwaitingSnapshot { started_at: Utc::now() },
        };
        Self { chain, venue, epoch, latest: RwLock::new(latest) }
    }

    /// The chain the service serves.
    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// The venue this stream serves.
    pub fn venue(&self) -> RfqVenue {
        self.venue
    }

    /// The `chain` and `venue` label values every metric of this stream carries.
    pub fn labels(&self) -> Vec<Label> {
        vec![
            Label::new("chain", self.chain.to_string()),
            Label::new("venue", self.venue.to_string()),
        ]
    }

    /// The latest message, or `None` while no snapshot has arrived in this run.
    pub fn current(&self) -> Option<LatestMessage> {
        let latest = self.read();
        latest
            .encoded
            .clone()
            .map(|encoded| LatestMessage { epoch: self.epoch, seq: latest.seq, encoded })
    }

    /// Whether the stream holds prices rather than a withdrawal or nothing yet.
    pub fn has_prices(&self) -> bool {
        match self.read().state {
            StreamState::Serving { .. } => true,
            StreamState::AwaitingSnapshot { .. } | StreamState::Withdrawn { .. } => false,
        }
    }

    /// Replaces the latest message with a snapshot of `prices`, received from the venue at
    /// `as_of`.
    pub fn publish<P: Serialize>(&self, prices: &P, as_of: DateTime<Utc>) {
        let seq = self.read().seq + 1;
        let body = MessageBody::Snapshot { as_of, data: prices };
        let encoded = self.encode(&NexusMessage::new(self.epoch, seq, body));
        *self.write() =
            StreamRecord { seq, encoded: Some(encoded), state: StreamState::Serving { as_of } };
    }

    /// Replaces the latest message with a withdrawal, unless it already is one.
    pub fn withdraw(&self) {
        let (seq, last_as_of) = {
            let latest = self.read();
            let last_as_of = match latest.state {
                StreamState::Withdrawn { .. } => return,
                StreamState::AwaitingSnapshot { started_at: time } |
                StreamState::Serving { as_of: time } => time,
            };
            (latest.seq + 1, last_as_of)
        };
        let message = NexusMessage::<()>::new(self.epoch, seq, MessageBody::Withdrawn);
        let encoded = self.encode(&message);
        *self.write() = StreamRecord {
            seq,
            encoded: Some(encoded),
            state: StreamState::Withdrawn { last_as_of },
        };
    }

    /// Encodes `message` outside the lock and records its size.
    fn encode<D: Serialize>(&self, message: &NexusMessage<D>) -> Bytes {
        let encoded = message
            .encode()
            .expect("a stream message of serializable state encodes to JSON");
        histogram!("nexus_message_bytes", self.labels()).record(encoded.len() as f64);
        Bytes::from(encoded)
    }

    /// Sets `nexus_snapshot_age_seconds` to the age of the current prices. Without current
    /// prices, measures the time since the last prices arrived, or since startup if none have
    /// arrived.
    pub fn report_snapshot_age(&self, now: DateTime<Utc>) {
        let newest = self.read().state.newest_as_of();
        let age = (now - newest)
            .to_std()
            .unwrap_or(Duration::ZERO);
        gauge!("nexus_snapshot_age_seconds", self.labels()).set(age.as_secs_f64());
    }

    fn read(&self) -> RwLockReadGuard<'_, StreamRecord> {
        self.latest
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, StreamRecord> {
        self.latest
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Refreshes `nexus_snapshot_age_seconds` of every stream in `streams` on a timer.
pub fn spawn_snapshot_age_reports(streams: Vec<Arc<LatestPrices>>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(AGE_REPORT_INTERVAL);
        loop {
            ticker.tick().await;
            let now = Utc::now();
            for stream in &streams {
                stream.report_snapshot_age(now);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use serde_json::{json, Value};

    use super::*;

    const EPOCH: u64 = 7;

    fn latest_prices() -> LatestPrices {
        LatestPrices::new(Chain::Base, RfqVenue::Hashflow, EPOCH)
    }

    fn message(latest_prices: &LatestPrices) -> NexusMessage<Value> {
        let latest = latest_prices
            .current()
            .expect("the stream has a message");
        NexusMessage::decode(std::str::from_utf8(&latest.encoded).unwrap()).unwrap()
    }

    #[test]
    fn test_new_stream() {
        let latest_prices = latest_prices();

        assert!(!latest_prices.has_prices());
        assert!(latest_prices.current().is_none());
    }

    #[test]
    fn test_seq_grows_with_every_change_and_not_otherwise() {
        let latest_prices = latest_prices();
        let at = Utc::now();

        latest_prices.publish(&json!({"n": 1}), at);
        latest_prices.publish(&json!({"n": 2}), at);
        latest_prices.withdraw();
        latest_prices.withdraw();
        latest_prices.publish(&json!({"n": 3}), at);

        let NexusMessage { seq, body: MessageBody::Snapshot { data, as_of }, .. } =
            message(&latest_prices)
        else {
            panic!("expected a snapshot");
        };
        assert_eq!((seq, data, as_of), (4, json!({"n": 3}), at));
    }

    #[test]
    fn test_withdraw_before_first_snapshot() {
        let latest_prices = latest_prices();

        latest_prices.withdraw();

        assert!(!latest_prices.has_prices());
        assert_eq!(message(&latest_prices), NexusMessage::new(EPOCH, 1, MessageBody::Withdrawn));
    }

    #[test]
    fn test_etag_follows_epoch_and_seq() {
        let latest_prices = latest_prices();
        latest_prices.publish(&json!({}), Utc::now());
        let first = latest_prices.current().unwrap();
        latest_prices.publish(&json!({}), Utc::now());
        let second = latest_prices.current().unwrap();

        assert_eq!(first.etag(), "\"7-1\"");
        assert_eq!(second.etag(), "\"7-2\"");
    }

    #[test]
    fn test_snapshot_age_gauge_keeps_counting_while_not_serving() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let latest_prices = latest_prices();
        let now = Utc::now();
        let age_gauge = || {
            let (_, _, _, value) = snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .find(|(key, ..)| key.key().name() == "nexus_snapshot_age_seconds")
                .expect("the gauge is set");
            let DebugValue::Gauge(value) = value else { panic!("expected a gauge") };
            value.into_inner()
        };

        metrics::with_local_recorder(&recorder, || {
            latest_prices.publish(&json!({}), now - chrono::Duration::seconds(2));
            latest_prices.report_snapshot_age(now);
            assert_eq!(age_gauge(), 2.0);

            latest_prices.withdraw();
            latest_prices.report_snapshot_age(now + chrono::Duration::seconds(10));
            assert_eq!(age_gauge(), 12.0);
        });
    }
}
