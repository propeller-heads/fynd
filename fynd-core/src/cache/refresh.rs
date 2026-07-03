//! Background refresh of live quote-cache entries (ENG-6237, TECH_DOC §3 Phase B / B3).
//!
//! On every new block the [`RefreshScheduler`](crate::RefreshScheduler) re-solves each live
//! [`CacheEntry`](crate::cache::CacheEntry) with a generous solver budget (long timeout, wait for
//! every pool) so a repeat request hits a best-quality route computed against the current block
//! rather than the block the entry was first solved at. Refreshes run at most `K` concurrent behind
//! a semaphore; a cycle is shed entirely when live queue depth already saturates the pools, and a
//! cycle's not-yet-started refreshes are abandoned the instant the next block arrives so cycle N's
//! leftovers never queue behind cycle N+1.
//!
//! The scheduler drives the router through [`RefreshRouter`](crate::RefreshRouter) rather than a
//! concrete [`WorkerPoolRouter`](crate::WorkerPoolRouter) so tests can inject a counting fake and
//! assert shedding, abandonment, and the concurrency bound without a live solver.

use std::sync::Arc;

use async_trait::async_trait;
use metrics::counter;
use tokio::{
    sync::{broadcast, watch, Semaphore},
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, info, warn};

use crate::{
    cache::CacheEntry, feed::market_data::MarketData, worker_pool_router::WorkerPoolRouter,
    MarketEvent, QuoteCache, QuoteRequest, QuoteStatus, SolveError, SolvedQuote,
};

/// Tunables for the refresh scheduler. Defaults match TECH_DOC §3 B3; all overridable, the same
/// way [`QuoteCachePolicy`](super::QuoteCachePolicy) is.
#[derive(Clone, Copy, Debug)]
pub struct RefreshConfig {
    /// Maximum refreshes solved concurrently within a single block's cycle.
    pub max_concurrent: usize,
    /// Skip a whole cycle when the summed live queue depth across pools exceeds this.
    pub shed_threshold: usize,
    /// Per-refresh solver timeout — generous, since a refresh runs off the request path.
    pub timeout_ms: u64,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self { max_concurrent: 4, shed_threshold: 50, timeout_ms: 5000 }
    }
}

/// The slice of the router the refresh scheduler needs, abstracted for testing.
///
/// [`WorkerPoolRouter`] is the production implementor; tests supply a counting fake.
#[async_trait]
pub trait RefreshRouter: Send + Sync + 'static {
    /// Solves a request without encoding, returning the ranked pre-encode quotes.
    async fn solve(&self, request: QuoteRequest) -> Result<SolvedQuote, SolveError>;

    /// Number of solver pools; a refresh waits for all of them (`min_responses`).
    fn num_pools(&self) -> usize;

    /// Summed approximate depth of pending solve tasks across all pools.
    fn queue_depth(&self) -> usize;

    /// Publishes per-pool live queue depth to the `worker_pool_queue_depth` gauge.
    fn record_queue_depth_gauge(&self);
}

#[async_trait]
impl RefreshRouter for WorkerPoolRouter {
    async fn solve(&self, request: QuoteRequest) -> Result<SolvedQuote, SolveError> {
        WorkerPoolRouter::solve(self, request).await
    }

    fn num_pools(&self) -> usize {
        WorkerPoolRouter::num_pools(self)
    }

    fn queue_depth(&self) -> usize {
        WorkerPoolRouter::queue_depth(self)
    }

    fn record_queue_depth_gauge(&self) {
        WorkerPoolRouter::record_queue_depth_gauge(self);
    }
}

/// Keeps live quote-cache entries fresh, one refresh cycle per block.
pub struct RefreshScheduler<R: RefreshRouter> {
    router: Arc<R>,
    cache: Arc<QuoteCache>,
    market_data: MarketData,
    config: RefreshConfig,
}

impl<R: RefreshRouter> RefreshScheduler<R> {
    /// Creates a scheduler over the given router, shared cache, chain-head source, and config.
    pub fn new(
        router: Arc<R>,
        cache: Arc<QuoteCache>,
        market_data: MarketData,
        config: RefreshConfig,
    ) -> Self {
        Self { router, cache, market_data, config }
    }

    /// Spawns [`run`](Self::run) as a background task, returning its handle.
    pub fn spawn(self, events: broadcast::Receiver<MarketEvent>) -> JoinHandle<()> {
        tokio::spawn(self.run(events))
    }

    /// Runs one refresh cycle per market event until the event channel closes.
    ///
    /// A lagged receiver means a missed block — one stale block, not an error — so it logs and
    /// continues. A closed channel means the feed has stopped, so the scheduler shuts down.
    pub async fn run(self, mut events: broadcast::Receiver<MarketEvent>) {
        // Cancellation handle for the block cycle currently in flight, replaced each block.
        let mut cancel_current: Option<watch::Sender<bool>> = None;

        loop {
            match events.recv().await {
                Ok(_event) => {
                    // Abandon the previous cycle: its unstarted refreshes must not run behind this
                    // block's cycle.
                    if let Some(previous) = cancel_current.take() {
                        let _ = previous.send(true);
                    }

                    let Some(head) = self.current_head().await else {
                        // Feed not synced yet: no head to refresh against.
                        continue;
                    };

                    let (cancel_tx, cancel_rx) = watch::channel(false);
                    cancel_current = Some(cancel_tx);
                    tokio::spawn(run_cycle(
                        Arc::clone(&self.router),
                        Arc::clone(&self.cache),
                        self.config,
                        head,
                        cancel_rx,
                    ));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "refresh scheduler lagged behind market events; skipping ahead");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    if let Some(previous) = cancel_current.take() {
                        let _ = previous.send(true);
                    }
                    info!("market event channel closed; refresh scheduler stopping");
                    break;
                }
            }
        }
    }

    /// Returns the number of the most recent block the feed has processed, if any.
    async fn current_head(&self) -> Option<u64> {
        self.market_data
            .read()
            .await
            .last_updated()
            .map(|block| block.number())
    }
}

/// Refreshes every live entry once, at most `config.max_concurrent` in flight.
///
/// Sheds the whole cycle when live queue depth is over the threshold. Skips entries already solved
/// at (or past) `head`. Stops starting new refreshes the moment `cancel` fires; in-flight refreshes
/// finish and self-discard.
async fn run_cycle<R: RefreshRouter>(
    router: Arc<R>,
    cache: Arc<QuoteCache>,
    config: RefreshConfig,
    head: u64,
    mut cancel: watch::Receiver<bool>,
) {
    // Publish depth even on a shed cycle so the autoscaling gauge never goes stale.
    router.record_queue_depth_gauge();

    let depth = router.queue_depth();
    if depth > config.shed_threshold {
        counter!("quote_cache_refresh_sheds_total").increment(1);
        debug!(depth, threshold = config.shed_threshold, "shedding refresh cycle: pools saturated");
        return;
    }

    let min_responses = router.num_pools();
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
    let mut refreshes = JoinSet::new();

    for (_key, entry) in cache.snapshot() {
        // Already current: refreshing would spend a solve to recompute the same block.
        if entry.solved_at_block() >= head {
            continue;
        }

        // Bound concurrency, but abandon the acquire the instant the next block cancels this cycle
        // so no new refresh from this cycle starts.
        let permit = tokio::select! {
            biased;
            _ = cancel.changed() => break,
            permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };

        let router = Arc::clone(&router);
        let cache = Arc::clone(&cache);
        let cancel = cancel.clone();
        refreshes.spawn(async move {
            let _permit = permit;
            refresh_entry(
                router.as_ref(),
                cache.as_ref(),
                entry,
                min_responses,
                config.timeout_ms,
                &cancel,
            )
            .await;
        });
    }

    // Drain in-flight refreshes. They self-discard if this cycle was cancelled; because the next
    // cycle runs in its own task, draining here never blocks the following block.
    while refreshes.join_next().await.is_some() {}
}

/// Re-solves one entry with a generous budget and replaces it on success.
///
/// On solver failure or a non-success quote the previous cached entry is kept (counted at debug).
/// If the cycle was cancelled while this refresh was solving, the now-stale result is discarded
/// rather than allowed to overwrite a fresher entry from the next cycle.
async fn refresh_entry<R: RefreshRouter>(
    router: &R,
    cache: &QuoteCache,
    entry: CacheEntry,
    min_responses: usize,
    timeout_ms: u64,
    cancel: &watch::Receiver<bool>,
) {
    let stored_request = entry.request().clone();
    let Some(order) = stored_request.orders().first().cloned() else {
        // Only single-order requests are ever cached; guard rather than index blindly.
        return;
    };

    let options = stored_request
        .options()
        .clone()
        .with_timeout_ms(timeout_ms)
        .with_min_responses(min_responses);
    let refresh_request = QuoteRequest::new(stored_request.orders().to_vec(), options);

    let solved = match router.solve(refresh_request).await {
        Ok(solved) => solved,
        Err(error) => {
            counter!("quote_cache_refresh_failures_total").increment(1);
            debug!(%error, "refresh solve failed; keeping previous cached quote");
            return;
        }
    };

    let Some(candidate) = solved.order_quotes().first().cloned() else {
        counter!("quote_cache_refresh_failures_total").increment(1);
        debug!("refresh solve returned no quote; keeping previous cached quote");
        return;
    };
    if candidate.status() != QuoteStatus::Success {
        counter!("quote_cache_refresh_failures_total").increment(1);
        debug!("refresh solve did not succeed; keeping previous cached quote");
        return;
    }

    // Abandonment: a result from a cancelled cycle is one block stale — discard it.
    if *cancel.borrow() {
        return;
    }

    let solved_at_block = candidate.block().number();
    cache.insert(&order, candidate, stored_request, entry.api_key_identity(), solved_at_block);
    counter!("quote_cache_refresh_solves_total").increment(1);
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };

    use num_bigint::BigUint;
    use tokio::sync::mpsc;
    use tycho_simulation::tycho_common::{models::Address, Bytes};

    use super::*;
    use crate::{
        BlockInfo, Order, OrderQuote, OrderSide, QuoteCachePolicy, QuoteOptions, QuoteStatus,
    };

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn order(sender: u8, amount: u64) -> Order {
        Order::new(addr(0x01), addr(0x02), BigUint::from(amount), OrderSide::Sell, addr(sender))
    }

    /// A successful pre-encode quote solved at `block`.
    fn quote_at(block: u64) -> OrderQuote {
        OrderQuote::new(
            "order-id".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(100u64),
            BigUint::from(900u64),
            BlockInfo::new(block, "0xhash".to_string(), block),
            "algo".to_string(),
            Bytes::from(addr(0x01).as_ref()),
            Bytes::from(addr(0x01).as_ref()),
            "1".to_string(),
        )
    }

    fn request(order: Order) -> QuoteRequest {
        QuoteRequest::new(vec![order], QuoteOptions::default())
    }

    /// Seeds `count` distinct single-order entries, each solved at `block`, into a fresh cache.
    fn cache_with_entries(count: u8, block: u64) -> Arc<QuoteCache> {
        let cache = Arc::new(QuoteCache::new(QuoteCachePolicy::default()));
        for i in 0..count {
            let order = order(0xA0 + i, 1000);
            cache.insert(&order, quote_at(block), request(order.clone()), "id-1", block);
        }
        cache
    }

    /// Instrumented router: counts solves, tracks peak concurrency, and can gate and fail solves.
    struct FakeRouter {
        num_pools: usize,
        queue_depth: usize,
        /// When set, each solve consumes one permit before returning — the test releases them to
        /// control when (and how many) solves complete.
        gate: Option<Arc<Semaphore>>,
        /// When set, each solve signals here as it begins.
        started_tx: Option<mpsc::UnboundedSender<()>>,
        fail: bool,
        result_block: u64,
        solves: AtomicUsize,
        concurrent: AtomicUsize,
        max_concurrent: AtomicUsize,
    }

    impl FakeRouter {
        /// A router that solves immediately (no gate), succeeding unless `fail`.
        fn immediate(num_pools: usize, queue_depth: usize, fail: bool, result_block: u64) -> Self {
            Self {
                num_pools,
                queue_depth,
                gate: None,
                started_tx: None,
                fail,
                result_block,
                solves: AtomicUsize::new(0),
                concurrent: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(0),
            }
        }

        /// A router whose solves block on `gate` and signal `started_tx`, for concurrency and
        /// abandonment control.
        fn gated(
            num_pools: usize,
            gate: Arc<Semaphore>,
            started_tx: mpsc::UnboundedSender<()>,
            result_block: u64,
        ) -> Self {
            Self {
                num_pools,
                queue_depth: 0,
                gate: Some(gate),
                started_tx: Some(started_tx),
                fail: false,
                result_block,
                solves: AtomicUsize::new(0),
                concurrent: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(0),
            }
        }

        fn solve_count(&self) -> usize {
            self.solves.load(Ordering::SeqCst)
        }

        fn peak_concurrency(&self) -> usize {
            self.max_concurrent
                .load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RefreshRouter for FakeRouter {
        async fn solve(&self, _request: QuoteRequest) -> Result<SolvedQuote, SolveError> {
            self.solves
                .fetch_add(1, Ordering::SeqCst);
            let now = self
                .concurrent
                .fetch_add(1, Ordering::SeqCst) +
                1;
            self.max_concurrent
                .fetch_max(now, Ordering::SeqCst);
            if let Some(started) = &self.started_tx {
                let _ = started.send(());
            }
            if let Some(gate) = &self.gate {
                gate.acquire()
                    .await
                    .expect("gate open")
                    .forget();
            }
            self.concurrent
                .fetch_sub(1, Ordering::SeqCst);
            if self.fail {
                return Err(SolveError::Internal("fake failure".to_string()));
            }
            Ok(SolvedQuote::new(vec![quote_at(self.result_block)], Instant::now()))
        }

        fn num_pools(&self) -> usize {
            self.num_pools
        }

        fn queue_depth(&self) -> usize {
            self.queue_depth
        }

        fn record_queue_depth_gauge(&self) {}
    }

    /// A cancellation channel that never fires. The returned sender must stay in scope for the
    /// duration of the cycle: dropping it closes the channel, which the cycle reads as a cancel.
    fn never_cancel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    #[tokio::test]
    async fn shed_skips_entire_cycle_when_depth_over_threshold() {
        let router = Arc::new(FakeRouter::immediate(2, 51, false, 100));
        let cache = cache_with_entries(3, 99);
        let config = RefreshConfig { shed_threshold: 50, ..Default::default() };

        let (_cancel_guard, cancel) = never_cancel();
        run_cycle(Arc::clone(&router), Arc::clone(&cache), config, 100, cancel).await;

        assert_eq!(router.solve_count(), 0, "no refreshes when shedding");
        assert_eq!(cache.snapshot().len(), 3, "entries untouched");
    }

    #[tokio::test]
    async fn skips_entries_already_at_current_block() {
        let router = Arc::new(FakeRouter::immediate(2, 0, false, 100));
        let cache = Arc::new(QuoteCache::new(QuoteCachePolicy::default()));
        let stale = order(0xAA, 1000);
        cache.insert(&stale, quote_at(99), request(stale.clone()), "id-1", 99);
        let current = order(0xBB, 1000);
        cache.insert(&current, quote_at(100), request(current.clone()), "id-1", 100);

        let (_cancel_guard, cancel) = never_cancel();
        run_cycle(Arc::clone(&router), Arc::clone(&cache), RefreshConfig::default(), 100, cancel)
            .await;

        assert_eq!(router.solve_count(), 1, "only the stale entry is re-solved");
    }

    #[tokio::test]
    async fn failed_resolve_keeps_previous_entry() {
        let router = Arc::new(FakeRouter::immediate(2, 0, true, 100));
        let cache = Arc::new(QuoteCache::new(QuoteCachePolicy::default()));
        let order = order(0xAA, 1000);
        cache.insert(&order, quote_at(99), request(order.clone()), "id-1", 99);

        let (_cancel_guard, cancel) = never_cancel();
        run_cycle(Arc::clone(&router), Arc::clone(&cache), RefreshConfig::default(), 100, cancel)
            .await;

        let kept = cache
            .get(&order, 100)
            .expect("entry still present");
        assert_eq!(kept.block().number(), 99, "previous solve preserved on failure");
    }

    #[tokio::test]
    async fn concurrency_never_exceeds_k() {
        let gate = Arc::new(Semaphore::new(0));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let router = Arc::new(FakeRouter::gated(2, Arc::clone(&gate), started_tx, 100));
        let cache = cache_with_entries(5, 99);
        let config = RefreshConfig { max_concurrent: 2, ..Default::default() };

        let (_cancel_guard, cancel) = never_cancel();
        let cycle =
            tokio::spawn(run_cycle(Arc::clone(&router), Arc::clone(&cache), config, 100, cancel));

        // Exactly K solves may enter concurrently; the rest wait on the semaphore, not on the gate.
        started_rx.recv().await.unwrap();
        started_rx.recv().await.unwrap();
        assert_eq!(router.solve_count(), 2, "K solves in flight, no more");

        gate.add_permits(10);
        cycle.await.unwrap();

        assert_eq!(router.solve_count(), 5, "every entry eventually refreshed");
        assert!(router.peak_concurrency() <= 2, "concurrency stayed within K");
    }

    #[tokio::test]
    async fn abandons_unstarted_refreshes_on_cancel_then_next_cycle_covers_all() {
        let gate = Arc::new(Semaphore::new(0));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let router = Arc::new(FakeRouter::gated(1, Arc::clone(&gate), started_tx, 100));
        let cache = cache_with_entries(3, 99);
        let config = RefreshConfig { max_concurrent: 1, ..Default::default() };

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let cycle = tokio::spawn(run_cycle(
            Arc::clone(&router),
            Arc::clone(&cache),
            config,
            100,
            cancel_rx,
        ));

        // One refresh is in flight; cancel before it finishes so the other two never start.
        started_rx.recv().await.unwrap();
        cancel_tx.send(true).unwrap();
        gate.add_permits(1);
        cycle.await.unwrap();
        assert_eq!(router.solve_count(), 1, "only the in-flight refresh ran");

        // Next block's cycle refreshes the full snapshot (the abandoned one self-discarded).
        gate.add_permits(10);
        let (_cancel_guard, cancel) = never_cancel();
        run_cycle(Arc::clone(&router), Arc::clone(&cache), config, 100, cancel).await;
        assert_eq!(router.solve_count(), 4, "next cycle covered all three entries");
    }
}
