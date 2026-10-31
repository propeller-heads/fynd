//! Runs one venue's prices feed and publishes it into a [`LatestStream`]. When the feed ends, the
//! stream is withdrawn and the feed starts again after [`RESTART_BACKOFF`], unless it ended with a
//! [`FeedError::Fatal`] or a [`FeedError::InvalidInput`]: then the runner stops for good.

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use futures::StreamExt as _;
use metrics::counter;
use serde::Serialize;
use tracing::{error, info, warn};
use tycho_simulation::snapshot_feed::{
    errors::FeedError, SnapshotFeed, SnapshotFeedEvent, SnapshotFeedOutcome, SnapshotFeedStream,
};

use crate::latest_stream::LatestStream;

/// How long to wait before starting a feed again after it ended.
pub const RESTART_BACKOFF: Duration = Duration::from_secs(30);

/// What to do about a feed that ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartDecision {
    /// Nothing a restart could change: the venue refuses the chain or the configuration is wrong.
    Stop,
    /// Start the feed again after a backoff.
    Restart,
}

/// Decides whether a feed that ended starts again or stops for good.
///
/// Stops after [`FeedError::Fatal`] (the venue refuses the chain or the handshake cannot be built)
/// or [`FeedError::InvalidInput`] (the feed configuration contains an unsupported value). Restarts
/// after repeated connection or parse failures, exhaustion, or a panic.
fn restart_decision(outcome: &SnapshotFeedOutcome<FeedError>) -> RestartDecision {
    match outcome {
        SnapshotFeedOutcome::Failed(error) => restart_decision_for_error(error),
        SnapshotFeedOutcome::RanOut | SnapshotFeedOutcome::Panicked(_) => RestartDecision::Restart,
    }
}

fn restart_decision_for_error(error: &FeedError) -> RestartDecision {
    match error {
        FeedError::Fatal(_) | FeedError::InvalidInput(_) => RestartDecision::Stop,
        FeedError::Connection(_) | FeedError::Parsing(_) => RestartDecision::Restart,
    }
}

/// Publishes everything the feed made by `start_feed` yields to `latest_stream`, and starts a new
/// feed when it ends, until one ends for good.
///
/// The stream is withdrawn when the feed's prices go stale and when the feed ends. After an end the
/// next feed starts after `restart_backoff`, unless the feed stops for good: a
/// [`FeedError::Fatal`] or [`FeedError::InvalidInput`] from the feed or from starting it. That is
/// logged at error level, counted in `nexus_venue_ended_total`, and leaves the stream
/// withdrawn for the rest of the process. Each restart is counted in
/// `nexus_venue_restarts_total`.
pub async fn run_feed<Feed, Start>(
    latest_stream: Arc<LatestStream>,
    mut start_feed: Start,
    restart_backoff: Duration,
) where
    Feed: SnapshotFeed<Error = FeedError>,
    Feed::Snapshot: Serialize + Clone + Send + Sync + 'static,
    Start: FnMut() -> Result<Feed, FeedError>,
{
    let venue = latest_stream.kind();
    loop {
        let decision = match start_feed() {
            Ok(feed) => follow_feed(&latest_stream, feed).await,
            Err(error) => {
                warn!(%venue, "cannot start the feed: {error}");
                restart_decision_for_error(&error)
            }
        };
        match decision {
            RestartDecision::Stop => {
                error!(%venue, "feed ended for good; the stream stays withdrawn");
                counter!("nexus_venue_ended_total", latest_stream.labels()).increment(1);
                return;
            }
            RestartDecision::Restart => {
                counter!("nexus_venue_restarts_total", latest_stream.labels()).increment(1);
                info!(%venue, ?restart_backoff, "restarting the feed");
                tokio::time::sleep(restart_backoff).await;
            }
        }
    }
}

/// Runs `feed`, publishes its snapshots and withdrawals, and returns whether to restart after it
/// ends.
async fn follow_feed<Feed>(latest_stream: &LatestStream, feed: Feed) -> RestartDecision
where
    Feed: SnapshotFeed<Error = FeedError>,
    Feed::Snapshot: Serialize + Clone + Send + Sync + 'static,
{
    let venue = latest_stream.kind();
    let mut events = SnapshotFeedStream::spawn(&venue.to_string(), feed);
    while let Some(event) = events.next().await {
        match event {
            SnapshotFeedEvent::Published(prices) => latest_stream.publish(&prices, Utc::now()),
            SnapshotFeedEvent::Withdrawn => {
                warn!(%venue, "venue prices went stale; withdrawing");
                latest_stream.withdraw();
            }
            SnapshotFeedEvent::Ended(outcome) => {
                latest_stream.withdraw();
                match &outcome {
                    SnapshotFeedOutcome::Failed(error) => {
                        warn!(%venue, "feed ended with an error: {error}");
                    }
                    SnapshotFeedOutcome::Panicked(message) => {
                        error!(%venue, "feed panicked: {message}");
                    }
                    SnapshotFeedOutcome::RanOut => {
                        warn!(%venue, "feed has nothing left to publish");
                    }
                }
                return restart_decision(&outcome);
            }
        }
    }
    // The stream should yield `Ended` before closing. If the stream closes without `Ended`, the
    // runner withdraws its prices and restarts the feed.
    latest_stream.withdraw();
    RestartDecision::Restart
}
