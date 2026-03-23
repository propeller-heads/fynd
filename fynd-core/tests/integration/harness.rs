
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fynd_core::{
    algorithm::AlgorithmConfig,
    derived::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef},
    feed::{
        market_data::{SharedMarketData, SharedMarketDataRef},
        tycho_feed::TychoFeed,
        MarketEvent, TychoFeedConfig,
    },
    recording::MarketRecording,
    types::{constants::native_token, Order, Quote, QuoteOptions, QuoteRequest},
    worker_pool::pool::{WorkerPool, WorkerPoolBuilder},
    worker_pool_router::{SolverPoolHandle, WorkerPoolRouter},
    SolveError, WorkerPoolRouterConfig,
};
use num_bigint::BigUint;
use serde::Deserialize;
use tokio::sync::{broadcast, RwLock};
use tycho_simulation::{
    tycho_common::models::Chain,
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

/// The fully constructed test pipeline, ready to receive quote requests.
pub struct TestHarness {
    pub market_data: SharedMarketDataRef,
    pub derived_data: SharedDerivedDataRef,
    router: WorkerPoolRouter,
    _market_tx: broadcast::Sender<MarketEvent>,
    _shutdown_tx: broadcast::Sender<()>,
    _worker_pools: Vec<WorkerPool>,
    _cm_handle: tokio::task::JoinHandle<()>,
}

/// Minimal struct for parsing pool config from worker_pools.toml.
#[derive(Debug, Deserialize)]
struct PoolsFile {
    pools: HashMap<String, PoolEntry>,
}

#[derive(Debug, Deserialize)]
struct PoolEntry {
    algorithm: String,
    #[serde(default = "default_num_workers")]
    num_workers: usize,
    #[serde(default = "default_min_hops")]
    min_hops: usize,
    #[serde(default = "default_max_hops")]
    max_hops: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    max_routes: Option<usize>,
}

fn default_num_workers() -> usize { num_cpus::get() }
fn default_min_hops() -> usize { 1 }
fn default_max_hops() -> usize { 3 }
fn default_timeout_ms() -> u64 { 100 }

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
        let block_number = recording.last_block_number();
        let gas_price_wei: BigUint = recording
            .metadata
            .gas_price_wei
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| BigUint::from(10_000_000_000u64));

        // 1. Replay recording
        let market_data: SharedMarketDataRef =
            Arc::new(RwLock::new(SharedMarketData::new()));
        let feed_config = TychoFeedConfig::new(
            "ws://replay".to_string(),
            Chain::Ethereum,
            None,
            false,
            vec![],
            0.0,
        );
        let feed = TychoFeed::new(feed_config, market_data.clone());
        let _feed_rx = feed.subscribe();

        for recorded_update in recording.updates {
            let update = recorded_update.into();
            feed.handle_tycho_message(update)
                .await
                .expect("replay of recorded update failed");
        }

        // 2. Inject gas price
        {
            let mut market = market_data.write().await;
            market.update_gas_price(BlockGasPrice {
                block_number,
                block_hash: Default::default(),
                block_timestamp: 0,
                pricing: GasPrice::Legacy {
                    gas_price: gas_price_wei,
                },
            });
        }

        // 3. Create ComputationManager
        let gas_token = native_token(&Chain::Ethereum)
            .expect("ethereum native token must be configured");
        let config =
            ComputationManagerConfig::default().with_gas_token(gas_token);
        let (computation_manager, _derived_events_rx) =
            ComputationManager::new(config, market_data.clone())
                .expect("failed to create computation manager");
        let derived_data = computation_manager.store();
        let derived_event_tx = computation_manager.event_sender();

        let (market_tx, market_rx) = broadcast::channel(64);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let cm_handle =
            tokio::spawn(computation_manager.run(market_rx, shutdown_rx));

        // 4. Build worker pools from worker_pools.toml BEFORE sending
        // MarketUpdated, so workers receive DerivedDataEvent broadcasts.
        // Builds separate pools matching production (not merged).
        let pools_toml = include_str!("../../../worker_pools.toml");
        let pools_config: PoolsFile =
            toml::from_str(pools_toml).expect("failed to parse worker_pools.toml");

        let mut solver_pool_handles = Vec::new();
        let mut worker_pools = Vec::new();
        let mut max_timeout_ms = 0u64;

        for (name, pool_entry) in &pools_config.pools {
            let algo_config = AlgorithmConfig::new(
                pool_entry.min_hops,
                pool_entry.max_hops,
                Duration::from_millis(pool_entry.timeout_ms),
                pool_entry.max_routes,
            )
            .expect("invalid algorithm config from worker_pools.toml");

            let market_event_rx = market_tx.subscribe();
            let derived_event_rx = derived_event_tx.subscribe();

            let (worker_pool, task_handle) = WorkerPoolBuilder::new()
                .name(name.clone())
                .algorithm(pool_entry.algorithm.clone())
                .algorithm_config(algo_config)
                .num_workers(pool_entry.num_workers)
                .build(
                    market_data.clone(),
                    derived_data.clone(),
                    market_event_rx,
                    derived_event_rx,
                )
                .expect("failed to build worker pool");

            solver_pool_handles
                .push(SolverPoolHandle::new(worker_pool.name(), task_handle));
            max_timeout_ms = max_timeout_ms.max(pool_entry.timeout_ms);
            worker_pools.push(worker_pool);
        }

        let router_config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(max_timeout_ms.max(5000)))
            .with_min_responses(0);
        let router = WorkerPoolRouter::new(
            solver_pool_handles,
            router_config,
            default_test_encoder(),
        );

        // 5. Trigger derived data computation
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

        // 6. Wait for all derived data
        let timeout = tokio::time::sleep(Duration::from_secs(120));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = &mut timeout => {
                    panic!("derived data computation timed out after 120s");
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    let d = derived_data.read().await;
                    if d.spot_prices().is_some() && d.pool_depths().is_some() && d.token_prices().is_some() {
                        break;
                    }
                }
            }
        }

        // Give workers time to process the DerivedDataEvent broadcast.
        // The wait loop above detects that derived data exists in the store,
        // but workers receive the event via a broadcast channel and need a
        // moment to update their internal readiness state.
        tokio::time::sleep(Duration::from_secs(1)).await;

        Self {
            market_data,
            derived_data,
            router,
            _market_tx: market_tx,
            _shutdown_tx: shutdown_tx,
            _worker_pools: worker_pools,
            _cm_handle: cm_handle,
        }
    }

    /// Run a single quote request and return the result.
    pub async fn quote(&self, orders: Vec<Order>) -> Result<Quote, SolveError> {
        let request = QuoteRequest::new(orders, QuoteOptions::default());
        self.router.quote(request).await
    }
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
