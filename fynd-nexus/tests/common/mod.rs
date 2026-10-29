//! A scripted prices feed that replaces a venue connection in tests.

// Every test crate includes this module and uses its own part of it.
#![allow(dead_code)]

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures::StreamExt as _;
use fynd_core::nexus::{rfq::RfqVenue, NexusMessage};
use fynd_nexus::latest_prices::LatestPrices;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tycho_simulation::{
    evm::tycho_models::Chain,
    snapshot_feed::{errors::FeedError, Publisher, SnapshotFeed},
};

pub const EPOCH: u64 = 1_761_550_000_000;

enum Step<P> {
    Publish(P),
    Fail(FeedError),
    Panic,
}

/// A feed that publishes what the test sends it. A sent error ends the feed with that error, a
/// sent panic makes it panic, and dropping the controller ends it without an error.
pub struct FakeFeed<P> {
    script: UnboundedReceiverStream<Step<P>>,
    max_snapshot_age: Option<Duration>,
}

/// The controller a test uses to send prices, errors, or a panic to a [`FakeFeed`].
pub struct FeedControl<P>(mpsc::UnboundedSender<Step<P>>);

impl<P> FeedControl<P> {
    /// The feed publishes `prices`.
    pub fn publish(&self, prices: P) {
        self.0
            .send(Step::Publish(prices))
            .unwrap();
    }

    /// The feed ends with `error`.
    pub fn fail(&self, error: FeedError) {
        self.0.send(Step::Fail(error)).unwrap();
    }

    /// The feed panics.
    pub fn panic(&self) {
        self.0.send(Step::Panic).unwrap();
    }
}

/// A scripted feed that withdraws what it published after `max_snapshot_age`, and its controller.
pub fn scripted_feed<P>(max_snapshot_age: Option<Duration>) -> (FeedControl<P>, FakeFeed<P>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        FeedControl(sender),
        FakeFeed { script: UnboundedReceiverStream::new(receiver), max_snapshot_age },
    )
}

impl<P: Send + Sync + Clone + 'static> SnapshotFeed for FakeFeed<P> {
    type Snapshot = P;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<P>,
    ) -> impl std::future::Future<Output = Result<(), FeedError>> + Send + 'static {
        publisher.publishing(
            self.max_snapshot_age,
            self.script.map(|step| match step {
                Step::Publish(prices) => Ok(prices),
                Step::Fail(error) => Err(error),
                Step::Panic => panic!("the scripted feed panics"),
            }),
        )
    }
}

/// The result of starting a feed: the feed or its startup error.
pub type Attempt<P> = Result<FakeFeed<P>, FeedError>;

/// Scripted feed-start results, consumed in order.
pub struct Attempts<P> {
    queue: Arc<Mutex<VecDeque<Attempt<P>>>>,
    started: Arc<AtomicUsize>,
}

impl<P> Attempts<P> {
    pub fn new(attempts: Vec<Attempt<P>>) -> Self {
        Self {
            queue: Arc::new(Mutex::new(attempts.into())),
            started: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The number of calls to the feed starter.
    pub fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    /// The function the feed runner calls to start a feed. After consuming all scripted results, it
    /// returns a fatal error that ends the stream.
    pub fn starter(&self) -> impl FnMut() -> Attempt<P> + Send + 'static
    where
        P: Send + 'static,
    {
        let queue = Arc::clone(&self.queue);
        let started = Arc::clone(&self.started);
        move || {
            started.fetch_add(1, Ordering::SeqCst);
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(FeedError::Fatal("the script has no more feeds".into())))
        }
    }
}

pub fn new_latest_prices(venue: RfqVenue) -> Arc<LatestPrices> {
    Arc::new(LatestPrices::new(Chain::Ethereum, venue, EPOCH))
}

pub fn decode<D: DeserializeOwned>(encoded: &[u8]) -> NexusMessage<D> {
    NexusMessage::decode(std::str::from_utf8(encoded).unwrap()).unwrap()
}

/// The message a stream serves now.
pub fn current_message<D: DeserializeOwned>(latest_prices: &LatestPrices) -> NexusMessage<D> {
    decode(
        &latest_prices
            .current()
            .expect("the stream has a message")
            .encoded,
    )
}
