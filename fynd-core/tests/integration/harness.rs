
use std::sync::Arc;
use std::time::Duration;

use fynd_core::{
    derived::{ComputationManager, ComputationManagerConfig, DerivedDataEvent, SharedDerivedDataRef},
    feed::{
        market_data::{SharedMarketData, SharedMarketDataRef},
        tycho_feed::TychoFeed,
        MarketEvent, TychoFeedConfig,
    },
    recording::MarketRecording,
    types::{Order, Quote, QuoteOptions, QuoteRequest},
    worker_pool::pool::{WorkerPool, WorkerPoolBuilder},
    worker_pool_router::{SolverPoolHandle, WorkerPoolRouter},
    SolveError, WorkerPoolRouterConfig,
};
use tokio::sync::{broadcast, RwLock};
use tycho_simulation::tycho_common::models::Chain;

/// The fully constructed test pipeline, ready to receive quote requests.
pub struct TestHarness {
    pub market_data: SharedMarketDataRef,
    pub derived_data: SharedDerivedDataRef,
    router: WorkerPoolRouter,
    _shutdown_tx: broadcast::Sender<()>,
    _worker_pool: WorkerPool,
    _cm_handle: tokio::task::JoinHandle<()>,
}

impl TestHarness {
    /// Load recording from the fixtures directory and build the full pipeline.
    pub async fn from_fixture() -> Self {
        let recording_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/integration/market_recording.json.zst");

        let recording = fynd_core::recording::read_recording(&recording_path)
            .expect("failed to load market recording fixture");

        Self::from_recording(recording).await
    }

    /// Build the pipeline by replaying a recording through TychoFeed.
    pub async fn from_recording(recording: MarketRecording) -> Self {
        // 1. Create empty SharedMarketData + TychoFeed for replay
        let market_data: SharedMarketDataRef =
            Arc::new(RwLock::new(SharedMarketData::new()));
        let feed_config = TychoFeedConfig::new(
            "ws://replay".to_string(), // dummy URL — not connecting
            Chain::Ethereum,
            None,   // no API key needed for replay
            false,  // no TLS
            vec![], // no protocol filter — replay all
            0.0,    // no TVL filter — replay all
        );
        let feed = TychoFeed::new(feed_config, market_data.clone());

        // 2. Replay each recorded Update through handle_tycho_message
        for recorded_update in recording.updates {
            let update = recorded_update.into();
            feed.handle_tycho_message(update)
                .await
                .expect("replay of recorded update failed");
        }

        // 3. Create ComputationManager and compute derived data
        let gas_token = find_weth_address(&market_data).await;
        let config =
            ComputationManagerConfig::default().with_gas_token(gas_token);
        let (computation_manager, derived_events_rx) =
            ComputationManager::new(config, market_data.clone())
                .expect("failed to create computation manager");
        let derived_data = computation_manager.store();

        // 4. Run ComputationManager: send a synthetic MarketUpdated event
        let (market_tx, market_rx) = broadcast::channel(64);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let cm_handle =
            tokio::spawn(computation_manager.run(market_rx, shutdown_rx));

        // Build the "all components added" event from current market state
        let market_read = market_data.read().await;
        let added = market_read.component_topology();
        drop(market_read);

        market_tx
            .send(MarketEvent::MarketUpdated {
                added_components: added,
                removed_components: vec![],
                updated_components: vec![],
            })
            .expect("failed to send market event");

        // 5. Wait for derived data to be computed
        let timeout = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = &mut timeout => {
                    panic!("derived data computation timed out after 30s");
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    let d = derived_data.read().await;
                    if d.spot_prices().is_some() && d.token_prices().is_some() {
                        break;
                    }
                }
            }
        }

        // 6. Build worker pool + router
        let market_event_rx = market_tx.subscribe();
        let (pool_handle, worker_pool) = build_test_worker_pool(
            market_data.clone(),
            derived_data.clone(),
            market_event_rx,
            derived_events_rx,
        );

        let router_config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(5000));
        let router = WorkerPoolRouter::new(
            vec![pool_handle],
            router_config,
            default_test_encoder(),
        );

        Self {
            market_data,
            derived_data,
            router,
            _shutdown_tx: shutdown_tx,
            _worker_pool: worker_pool,
            _cm_handle: cm_handle,
        }
    }

    /// Run a single quote request and return the result.
    pub async fn quote(&self, orders: Vec<Order>) -> Result<Quote, SolveError> {
        let request = QuoteRequest::new(orders, QuoteOptions::default());
        self.router.quote(request).await
    }
}

/// Find the WETH address in the market data (used as gas token).
async fn find_weth_address(
    market_data: &SharedMarketDataRef,
) -> tycho_simulation::tycho_common::models::Address {
    let weth: tycho_simulation::tycho_common::models::Address =
        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .expect("invalid WETH address");
    let market = market_data.read().await;
    if market.token_registry_ref().contains_key(&weth) {
        return weth;
    }
    tycho_simulation::tycho_common::models::Address::zero(20)
}

/// Build a single test worker pool with the MostLiquid algorithm.
fn build_test_worker_pool(
    market_data: SharedMarketDataRef,
    derived_data: SharedDerivedDataRef,
    market_event_rx: broadcast::Receiver<MarketEvent>,
    derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
) -> (SolverPoolHandle, WorkerPool) {
    let (worker_pool, task_handle) = WorkerPoolBuilder::new()
        .algorithm("most_liquid")
        .num_workers(2)
        .build(market_data, derived_data, market_event_rx, derived_event_rx)
        .expect("failed to build test worker pool");

    (SolverPoolHandle::new("test_pool", task_handle), worker_pool)
}

/// Create an Encoder for the router.
fn default_test_encoder() -> fynd_core::encoding::encoder::Encoder {
    use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;

    let registry = SwapEncoderRegistry::new(Chain::Ethereum)
        .add_default_encoders(None)
        .expect("default encoders should always succeed");
    fynd_core::encoding::encoder::Encoder::new(Chain::Ethereum, registry)
        .expect("encoder creation should succeed")
}
