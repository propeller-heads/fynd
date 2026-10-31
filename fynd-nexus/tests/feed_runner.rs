mod common;

use std::time::Duration;

use common::{current_message, new_latest_stream, scripted_feed, Attempts, FeedControl, EPOCH};
use fynd_core::nexus::{rfq::RfqVenue, MessageBody, NexusMessage};
use fynd_nexus::{feed_runner::run_feed, latest_stream::LatestStream};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use rstest::rstest;
use serde_json::{json, Value};
use tokio::time::sleep;
use tycho_simulation::snapshot_feed::errors::FeedError;

const BACKOFF: Duration = Duration::from_secs(30);

enum FeedEnd {
    Error(FeedError),
    RanOut,
    Panic,
}

impl FeedEnd {
    fn end_feed(self, feed: FeedControl<Value>) {
        match self {
            Self::Error(error) => feed.fail(error),
            Self::RanOut => drop(feed),
            Self::Panic => feed.panic(),
        }
    }
}

fn counter(snapshotter: &Snapshotter, name: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(key, _, _, value)| match (key.key().name() == name, value) {
            (true, DebugValue::Counter(count)) => Some(count),
            _ => None,
        })
        .unwrap_or(0)
}

fn snapshot_data(latest_stream: &LatestStream) -> Value {
    let NexusMessage { body: MessageBody::Snapshot { data, .. }, .. } =
        current_message::<Value>(latest_stream)
    else {
        panic!("expected a snapshot");
    };
    data
}

#[tokio::test(start_paused = true)]
async fn test_publishes_what_the_feed_yields() {
    let latest_stream = new_latest_stream(RfqVenue::Bebop);
    let (control, feed) = scripted_feed(None);
    let attempts = Attempts::new(vec![Ok(feed)]);
    tokio::spawn(run_feed(latest_stream.clone(), attempts.starter(), BACKOFF));

    control.publish(json!({"pairs": [1]}));
    sleep(Duration::from_millis(10)).await;
    control.publish(json!({"pairs": [2]}));
    sleep(Duration::from_millis(10)).await;

    let latest = latest_stream.current().unwrap();
    assert!(latest_stream.is_serving());
    assert_eq!((latest.epoch, latest.seq), (EPOCH, 2));
    assert_eq!(snapshot_data(&latest_stream), json!({"pairs": [2]}));
}

#[tokio::test(start_paused = true)]
async fn test_withdraws_what_the_feed_lets_go_stale() {
    let latest_stream = new_latest_stream(RfqVenue::Bebop);
    let (control, feed) = scripted_feed(Some(Duration::from_secs(60)));
    let attempts = Attempts::new(vec![Ok(feed)]);
    tokio::spawn(run_feed(latest_stream.clone(), attempts.starter(), BACKOFF));
    control.publish(json!({}));
    sleep(Duration::from_secs(59)).await;
    assert!(latest_stream.is_serving());

    sleep(Duration::from_secs(2)).await;

    assert!(!latest_stream.is_serving());
    assert!(matches!(current_message::<Value>(&latest_stream).body, MessageBody::Withdrawn));
    assert_eq!(attempts.started(), 1, "going stale is not the feed ending");
}

#[tokio::test(start_paused = true)]
async fn test_feed_end_during_restart_backoff() {
    let latest_stream = new_latest_stream(RfqVenue::Hashflow);
    let (first, first_feed) = scripted_feed(None);
    let attempts = Attempts::new(vec![Ok(first_feed)]);
    tokio::spawn(run_feed(latest_stream.clone(), attempts.starter(), BACKOFF));
    first.publish(json!({"gen": 1}));
    sleep(Duration::from_secs(1)).await;

    first.fail(FeedError::Connection("reset".into()));
    sleep(Duration::from_secs(1)).await;

    assert!(!latest_stream.is_serving());
    assert_eq!(attempts.started(), 1);
}

#[rstest]
#[case::connection(FeedEnd::Error(FeedError::Connection("reset".into())))]
#[case::parsing(FeedEnd::Error(FeedError::Parsing("bad message".into())))]
#[case::ran_out(FeedEnd::RanOut)]
#[case::panicked(FeedEnd::Panic)]
#[tokio::test(start_paused = true)]
async fn test_feed_ending_that_restarts(#[case] feed_end: FeedEnd) {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let latest_stream = new_latest_stream(RfqVenue::Hashflow);
    let (first, first_feed) = scripted_feed(None);
    let (second, second_feed) = scripted_feed(None);
    let attempts = Attempts::new(vec![Ok(first_feed), Ok(second_feed)]);
    tokio::spawn(run_feed(latest_stream.clone(), attempts.starter(), BACKOFF));
    first.publish(json!({"gen": 1}));
    sleep(Duration::from_secs(1)).await;

    feed_end.end_feed(first);
    sleep(BACKOFF + Duration::from_secs(1)).await;
    second.publish(json!({"gen": 2}));
    sleep(Duration::from_secs(1)).await;

    assert_eq!(attempts.started(), 2);
    assert_eq!(snapshot_data(&latest_stream), json!({"gen": 2}));
    assert_eq!(counter(&snapshotter, "nexus_venue_restarts_total"), 1);
    assert_eq!(counter(&snapshotter, "nexus_venue_ended_total"), 0);
}

#[rstest]
#[case::fatal(FeedError::Fatal("venue refuses the chain".into()))]
#[case::invalid_input(FeedError::InvalidInput("zero age".into()))]
#[tokio::test(start_paused = true)]
async fn test_feed_ending_that_stops(#[case] error: FeedError) {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let latest_stream = new_latest_stream(RfqVenue::Hashflow);
    let (first, first_feed) = scripted_feed(None);
    let attempts = Attempts::new(vec![Ok(first_feed)]);
    let runner = tokio::spawn(run_feed(latest_stream.clone(), attempts.starter(), BACKOFF));
    first.publish(json!({"gen": 1}));
    sleep(Duration::from_secs(1)).await;

    first.fail(error);
    sleep(BACKOFF + Duration::from_secs(1)).await;

    assert!(runner.is_finished());
    assert_eq!(attempts.started(), 1);
    assert!(!latest_stream.is_serving());
    assert_eq!(counter(&snapshotter, "nexus_venue_ended_total"), 1);
    assert_eq!(counter(&snapshotter, "nexus_venue_restarts_total"), 0);
}

#[rstest]
#[case::connection(FeedError::Connection("dns".into()), true)]
#[case::fatal(FeedError::Fatal("unsupported chain".into()), false)]
#[case::invalid_input(FeedError::InvalidInput("bad url".into()), false)]
#[tokio::test(start_paused = true)]
async fn test_feed_that_cannot_start(#[case] error: FeedError, #[case] retries: bool) {
    let latest_stream = new_latest_stream(RfqVenue::Metric);
    let (control, feed) = scripted_feed(None);
    let attempts = Attempts::new(vec![Err(error), Ok(feed)]);
    let runner = tokio::spawn(run_feed(latest_stream.clone(), attempts.starter(), BACKOFF));

    sleep(BACKOFF + Duration::from_secs(1)).await;
    if retries {
        control.publish(json!({}));
        sleep(Duration::from_secs(1)).await;
    }

    assert_eq!(attempts.started(), if retries { 2 } else { 1 });
    assert_eq!(runner.is_finished(), !retries);
    assert_eq!(latest_stream.is_serving(), retries);
}
