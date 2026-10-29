//! Fynd Nexus and a pod-side RFQ prices feed reading from it over a local socket, with
//! a scripted feed standing in for Bebop.

mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use common::{scripted_feed, Attempts, FeedControl};
use futures::StreamExt as _;
use fynd_core::nexus::{
    client::NexusClient,
    rfq::{
        test_support::{bebop_assembler, bebop_prices},
        RfqAssembler, RfqVenue,
    },
};
use fynd_nexus::{
    api::{self, Streams},
    feed_runner::run_feed,
    latest_prices::LatestPrices,
};
use tokio::time::timeout;
use tycho_simulation::{
    book::{BookSnapshot, ReceivedAt},
    evm::tycho_models::Chain,
    rfq::protocols::bebop::BebopPrices,
    snapshot_feed::{
        errors::FeedError, SnapshotFeedEvent, SnapshotFeedOutcome, SnapshotFeedStream,
    },
};

const DEADLINE: Duration = Duration::from_secs(15);

type PodFeed = SnapshotFeedStream<BookSnapshot<ReceivedAt>, FeedError>;

/// A service run: its server, and the control of the feed behind its stream.
struct Service {
    address: SocketAddr,
    handle: actix_web::dev::ServerHandle,
    feed_controls: Vec<FeedControl<BebopPrices>>,
}

/// Starts a service with `epoch` on `address` and prepares `attempts` feeds. Starts the first feed
/// immediately and each remaining feed after its predecessor ends.
fn start_service(epoch: u64, address: SocketAddr, attempts: usize) -> Service {
    let latest_prices = Arc::new(LatestPrices::new(Chain::Ethereum, RfqVenue::Bebop, epoch));
    let mut feed_controls = Vec::new();
    let mut scripted = Vec::new();
    for _ in 0..attempts {
        let (control, feed) = scripted_feed(None);
        feed_controls.push(control);
        scripted.push(Ok(feed));
    }
    let attempts = Attempts::new(scripted);
    tokio::spawn(run_feed(latest_prices.clone(), attempts.starter(), Duration::from_millis(100)));

    let (server, address) = api::bind(Arc::new(Streams::new([latest_prices])), address).unwrap();
    let handle = server.handle();
    tokio::spawn(server);
    Service { address, handle, feed_controls }
}

fn start_pod(service: SocketAddr) -> PodFeed {
    let client = NexusClient::new(&format!("http://{service}")).unwrap();
    SnapshotFeedStream::spawn("pod", client.rfq_prices_feed(RfqAssembler::Bebop(bebop_assembler())))
}

/// The books the pod publishes next.
async fn next_books(pod: &mut PodFeed) -> BookSnapshot<ReceivedAt> {
    match timeout(DEADLINE, pod.next())
        .await
        .expect("the pod published nothing within the deadline")
    {
        Some(SnapshotFeedEvent::Published(books)) => books,
        Some(SnapshotFeedEvent::Withdrawn) => panic!("the pod withdrew the venue"),
        Some(SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Failed(error))) => {
            panic!("the pod's feed gave up: {error}")
        }
        Some(SnapshotFeedEvent::Ended(_)) | None => panic!("the pod's feed ended"),
    }
}

#[tokio::test]
async fn test_pod_follows_snapshot_withdrawal_snapshot_and_a_service_restart() {
    let service = start_service(1, "127.0.0.1:0".parse().unwrap(), 2);
    let mut pod = start_pod(service.address);
    let direct = bebop_assembler().books(bebop_prices(3000.1));

    service.feed_controls[0].publish(bebop_prices(3000.1));
    let books = next_books(&mut pod).await;
    let mut ids: Vec<_> = books.books.keys().collect();
    let mut direct_ids: Vec<_> = direct.keys().collect();
    ids.sort();
    direct_ids.sort();
    assert!(!ids.is_empty());
    assert_eq!(ids, direct_ids);
    assert!(
        (chrono::Utc::now() - books.anchor.0)
            .num_seconds()
            .abs() <
            5,
        "the books are anchored at the time the service received the prices"
    );

    service.feed_controls[0].fail(FeedError::Connection("venue went away".to_string()));
    assert!(next_books(&mut pod)
        .await
        .books
        .is_empty());
    service.feed_controls[1].publish(bebop_prices(3001.1));
    assert!(!next_books(&mut pod)
        .await
        .books
        .is_empty());

    // The pod accepts the restarted service's messages although the new epoch resets `seq`, and
    // keeps its books until the restarted service has a snapshot.
    service.handle.stop(false).await;
    let restarted = start_service(2, service.address, 1);
    restarted.feed_controls[0].publish(bebop_prices(3002.1));
    assert!(!next_books(&mut pod)
        .await
        .books
        .is_empty());
}
