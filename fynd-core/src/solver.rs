//! High-level solver setup via [`FyndBuilder`].
//!
//! [`FyndBuilder`] assembles the full Tycho feed + gas fetcher + computation
//! manager + one or more worker pools + encoder + router pipeline with sensible
//! defaults. For simple cases a single call chain is all that's needed:
//!
//! ```ignore
//! let solver = FyndBuilder::new(chain, tycho_url, rpc_url, protocols, min_tvl)
//!     .tycho_api_key(key)
//!     .algorithm("most_liquid")
//!     .build()?;
//! ```
use std::{collections::HashSet, str::FromStr, sync::Arc, time::Duration};

use num_cpus;
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast, task::JoinHandle};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
#[cfg(feature = "test-utils")]
use tycho_simulation::tycho_ethereum::gas::{BlockGasPrice, GasPrice};
use tycho_simulation::{
    evm::pending::PendingBlockProcessor,
    tycho_common::{models::Chain, traits::TxDeltaIndexer, Bytes},
    tycho_core::models::Address,
    tycho_ethereum::rpc::EthereumRpcClient,
};

use crate::{
    algorithm::{AlgorithmConfig, AlgorithmError},
    derived::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef},
    encoding::{encoder::Encoder, fee_fetcher::RouterFeeFetcher, router_fees::SharedRouterFees},
    feed::{
        events::{MarketEvent, MarketEventHandler},
        gas::GasPriceFetcher,
        market_data::MarketData,
        tycho_feed::TychoFeed,
        TychoFeedConfig,
    },
    graph::EdgeWeightUpdaterWithDerived,
    price_guard::{
        guard::PriceGuard, provider::PriceProvider, provider_registry::PriceProviderRegistry,
    },
    types::constants::native_token,
    worker_pool::{
        pool::{WorkerPool, WorkerPoolBuilder},
        registry::UnknownAlgorithmError,
    },
    worker_pool_router::{config::WorkerPoolRouterConfig, SolverPoolHandle, WorkerPoolRouter},
    Algorithm, Quote, QuoteRequest, SolveError,
};

/// Default values for [`FyndBuilder`] configuration and [`PoolConfig`] deserialization.
///
/// These are the single source of truth for all tunable defaults. Downstream
/// crates (e.g. `fynd-rpc`) should re-export or reference these rather than
/// redeclaring their own copies.
pub mod defaults {
    use std::time::Duration;

    /// Minimum token quality score required for a token to be included in routing.
    pub const MIN_TOKEN_QUALITY: i32 = 100;
    /// Maximum age (in days) of trading history required for a token to be considered liquid.
    pub const TRADED_N_DAYS_AGO: u64 = 3;
    /// Multiplier applied to a pool's TVL when estimating available liquidity.
    pub const TVL_BUFFER_RATIO: f64 = 1.1;
    /// How often the gas price is refreshed from the RPC node.
    pub const GAS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
    /// How often router fees are refreshed from the on-chain FeeCalculator contract.
    pub const ROUTER_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
    /// Delay before reconnecting to the Tycho feed after a disconnect.
    pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);
    /// Minimum number of solver pool responses required before returning a quote (`0` = wait for
    /// all).
    pub const ROUTER_MIN_RESPONSES: usize = 0;
    /// Capacity of the task queue for each worker pool.
    pub const POOL_TASK_QUEUE_CAPACITY: usize = 1000;
    /// Minimum number of hops allowed in a route.
    pub const POOL_MIN_HOPS: usize = 1;
    /// Maximum number of hops allowed in a route.
    pub const POOL_MAX_HOPS: usize = 3;
    /// Per-pool solve timeout in milliseconds.
    pub const POOL_TIMEOUT_MS: u64 = 100;
}

// Internal-only defaults not shared with downstream crates.
const DEFAULT_TYCHO_USE_TLS: bool = true;
const DEFAULT_DEPTH_SLIPPAGE_THRESHOLD: f64 = 0.01;
/// Generous router timeout for standalone (non-server) use. HTTP services should
/// override this to a tighter value appropriate for their SLA.
const DEFAULT_ROUTER_TIMEOUT: Duration = Duration::from_secs(10);

// serde requires free functions for `#[serde(default = "...")]` — these delegate to the
// defaults module so both deserialization and the builder stay in sync.
fn default_task_queue_capacity() -> usize {
    defaults::POOL_TASK_QUEUE_CAPACITY
}

fn default_min_hops() -> usize {
    defaults::POOL_MIN_HOPS
}

fn default_max_hops() -> usize {
    defaults::POOL_MAX_HOPS
}

fn default_algo_timeout_ms() -> u64 {
    defaults::POOL_TIMEOUT_MS
}

fn parse_connector_tokens(
    raw: Option<&[String]>,
) -> Result<Option<HashSet<Address>>, SolverBuildError> {
    let Some(strings) = raw else {
        return Ok(None);
    };
    let mut set = HashSet::with_capacity(strings.len());
    for s in strings {
        let addr = Address::from_str(s).map_err(|e| AlgorithmError::InvalidConfiguration {
            reason: format!("connector_tokens: invalid address {s:?}: {e}"),
        })?;
        set.insert(addr);
    }
    Ok(Some(set))
}

/// Per-pool configuration for [`FyndBuilder::add_pool`].
#[must_use]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Algorithm name for this pool (e.g., `"most_liquid"`).
    algorithm: String,
    /// Number of worker threads for this pool.
    #[serde(default = "num_cpus::get")]
    num_workers: usize,
    /// Task queue capacity for this pool.
    #[serde(default = "default_task_queue_capacity")]
    task_queue_capacity: usize,
    /// Minimum hops to search (must be >= 1).
    #[serde(default = "default_min_hops")]
    min_hops: usize,
    /// Maximum hops to search.
    #[serde(default = "default_max_hops")]
    max_hops: usize,
    /// Timeout for solving in milliseconds.
    #[serde(default = "default_algo_timeout_ms")]
    timeout_ms: u64,
    /// Maximum number of paths to simulate per solve. `None` simulates all scored paths.
    #[serde(default)]
    max_routes: Option<usize>,
    /// Lowercase hex addresses (e.g. `"0xc02aaa…"`) allowed as intermediate routing hops.
    /// Absent = no restriction. Typically 3–10 entries (e.g. WETH, USDC, USDT, DAI).
    #[serde(default)]
    connector_tokens: Option<Vec<String>>,
}

impl PoolConfig {
    /// Creates a new pool config with the given algorithm name and defaults for all other fields.
    pub fn new(algorithm: impl Into<String>) -> Self {
        Self {
            algorithm: algorithm.into(),
            num_workers: num_cpus::get(),
            task_queue_capacity: defaults::POOL_TASK_QUEUE_CAPACITY,
            min_hops: defaults::POOL_MIN_HOPS,
            max_hops: defaults::POOL_MAX_HOPS,
            timeout_ms: defaults::POOL_TIMEOUT_MS,
            max_routes: None,
            connector_tokens: None,
        }
    }

    /// Returns the algorithm name.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the number of worker threads.
    pub fn num_workers(&self) -> usize {
        self.num_workers
    }

    /// Sets the number of worker threads.
    pub fn with_num_workers(mut self, num_workers: usize) -> Self {
        self.num_workers = num_workers;
        self
    }

    /// Sets the task queue capacity.
    pub fn with_task_queue_capacity(mut self, task_queue_capacity: usize) -> Self {
        self.task_queue_capacity = task_queue_capacity;
        self
    }

    /// Sets the minimum hops.
    pub fn with_min_hops(mut self, min_hops: usize) -> Self {
        self.min_hops = min_hops;
        self
    }

    /// Sets the maximum hops.
    pub fn with_max_hops(mut self, max_hops: usize) -> Self {
        self.max_hops = max_hops;
        self
    }

    /// Sets the timeout in milliseconds.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Sets the maximum number of routes to simulate.
    pub fn with_max_routes(mut self, max_routes: Option<usize>) -> Self {
        self.max_routes = max_routes;
        self
    }

    /// Returns the task queue capacity.
    pub fn task_queue_capacity(&self) -> usize {
        self.task_queue_capacity
    }

    /// Returns the minimum hops.
    pub fn min_hops(&self) -> usize {
        self.min_hops
    }

    /// Returns the maximum hops.
    pub fn max_hops(&self) -> usize {
        self.max_hops
    }

    /// Returns the timeout in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Returns the maximum number of routes to simulate.
    pub fn max_routes(&self) -> Option<usize> {
        self.max_routes
    }

    /// Restricts intermediate hops to the given token addresses (hex strings with or without `0x`
    /// prefix). Absent = no restriction.
    pub fn with_connector_tokens(mut self, tokens: Vec<String>) -> Self {
        self.connector_tokens = Some(tokens);
        self
    }

    /// Returns the raw connector token address strings, or `None` if unrestricted.
    pub fn connector_tokens(&self) -> Option<&[String]> {
        self.connector_tokens.as_deref()
    }
}

/// Error returned by [`Solver::wait_until_ready`].
#[derive(Debug, thiserror::Error)]
#[error("timed out after {timeout_ms}ms waiting for market data and derived computations")]
pub struct WaitReadyError {
    timeout_ms: u64,
}

/// Error returned by [`FyndBuilder::build`] and [`FyndBuilder::build_with_pending`].
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SolverBuildError {
    /// The Ethereum RPC client could not be created (e.g. malformed URL).
    #[error("failed to create ethereum RPC client: {0}")]
    RpcClient(String),
    /// An invalid algorithm configuration was supplied.
    #[error(transparent)]
    AlgorithmConfig(#[from] AlgorithmError),
    /// The [`ComputationManager`] failed to initialise.
    #[error("failed to create computation manager: {0}")]
    ComputationManager(String),
    /// The swap encoder could not be created for the target chain.
    #[error("failed to create encoder: {0}")]
    Encoder(String),
    /// The router fee fetcher could not be created (e.g. malformed RPC URL).
    #[error("failed to create router fee fetcher: {0}")]
    RouterFeeFetcher(String),
    /// A pool referenced an algorithm name that is not registered.
    #[error(transparent)]
    UnknownAlgorithm(#[from] UnknownAlgorithmError),
    /// No native gas token is defined for the requested chain.
    #[error("gas token not configured for chain")]
    GasToken,
    /// [`FyndBuilder::build`] was called without configuring any worker pools.
    #[error("no worker pools configured")]
    NoPools,
    /// A recorded update failed to replay through the feed.
    #[cfg(feature = "test-utils")]
    #[error("replay failed: {0}")]
    Replay(String),
    /// The feed task failed before delivering the [`PendingBlockProcessor`].
    ///
    /// The inner string is the `DataFeedError` message from `TychoFeed::run_with_pending`
    /// (e.g. "failed to load tokens: connection refused").
    #[error("feed setup failed before delivering pending processor: {0}")]
    FeedSetup(String),
    /// The pending-processor oneshot closed without delivering a value, meaning the feed task
    /// panicked rather than returning an error through the channel.
    #[error("pending processor channel closed before processor was delivered")]
    PendingChannelClosed,
}

/// Internal pool entry — either a built-in algorithm (by name) or a custom one.
enum PoolEntry {
    BuiltIn {
        name: String,
        algorithm: String,
        num_workers: usize,
        task_queue_capacity: usize,
        min_hops: usize,
        max_hops: usize,
        timeout_ms: u64,
        max_routes: Option<usize>,
        connector_tokens: Option<HashSet<Address>>,
    },
    Custom(CustomPoolEntry),
}

/// Pool entry backed by a custom [`Algorithm`] implementation.
struct CustomPoolEntry {
    name: String,
    num_workers: usize,
    task_queue_capacity: usize,
    min_hops: usize,
    max_hops: usize,
    timeout_ms: u64,
    max_routes: Option<usize>,
    /// Applies the custom algorithm to a `WorkerPoolBuilder`.
    configure: Box<dyn FnOnce(WorkerPoolBuilder) -> WorkerPoolBuilder + Send>,
}

/// All components produced by [`FyndBuilder::assemble_components`], consumed by
/// [`FyndBuilder::build`] and [`FyndBuilder::build_with_pending`].
struct BuiltComponents {
    tycho_feed: TychoFeed,
    gas_price_fetcher: GasPriceFetcher<EthereumRpcClient>,
    router_fee_fetcher: RouterFeeFetcher,
    computation_manager: ComputationManager,
    computation_event_rx: broadcast::Receiver<MarketEvent>,
    computation_shutdown_tx: broadcast::Sender<()>,
    computation_shutdown_rx: broadcast::Receiver<()>,
    router: WorkerPoolRouter,
    worker_pools: Vec<WorkerPool>,
    market_data: MarketData,
    derived_data: SharedDerivedDataRef,
    router_fees: SharedRouterFees,
    chain: Chain,
    router_address: Bytes,
    pending_indexers: Vec<(String, Box<dyn TxDeltaIndexer>)>,
    market_event_tx: broadcast::Sender<MarketEvent>,
}

/// Builder for assembling the full solver pipeline.
///
/// Configures the Tycho market-data feed, gas price fetcher, derived-data
/// computation manager, one or more worker pools, encoder, and router.
#[must_use = "a builder does nothing until .build() is called"]
pub struct FyndBuilder {
    chain: Chain,
    tycho_url: String,
    rpc_url: String,
    protocols: Vec<String>,
    min_tvl: f64,
    tycho_api_key: Option<String>,
    tycho_use_tls: bool,
    min_token_quality: i32,
    traded_n_days_ago: u64,
    tvl_buffer_ratio: f64,
    gas_refresh_interval: Duration,
    reconnect_delay: Duration,
    blocklisted_components: HashSet<String>,
    partial_blocks: bool,
    router_timeout: Duration,
    router_min_responses: usize,
    encoder: Option<Encoder>,
    pools: Vec<PoolEntry>,
    price_guard_enabled: bool,
    price_providers: Vec<Box<dyn PriceProvider>>,
    pending_indexers: Vec<(String, Box<dyn TxDeltaIndexer>)>,
}

impl FyndBuilder {
    /// Creates a new builder with the required parameters.
    pub fn new(
        chain: Chain,
        tycho_url: impl Into<String>,
        rpc_url: impl Into<String>,
        protocols: Vec<String>,
        min_tvl: f64,
    ) -> Self {
        Self {
            chain,
            tycho_url: tycho_url.into(),
            rpc_url: rpc_url.into(),
            protocols,
            min_tvl,
            tycho_api_key: None,
            tycho_use_tls: DEFAULT_TYCHO_USE_TLS,
            min_token_quality: defaults::MIN_TOKEN_QUALITY,
            traded_n_days_ago: defaults::TRADED_N_DAYS_AGO,
            tvl_buffer_ratio: defaults::TVL_BUFFER_RATIO,
            gas_refresh_interval: defaults::GAS_REFRESH_INTERVAL,
            reconnect_delay: defaults::RECONNECT_DELAY,
            blocklisted_components: HashSet::new(),
            partial_blocks: false,
            router_timeout: DEFAULT_ROUTER_TIMEOUT,
            router_min_responses: defaults::ROUTER_MIN_RESPONSES,
            encoder: None,
            pools: Vec::new(),
            price_guard_enabled: false,
            price_providers: Vec::new(),
            pending_indexers: Vec::new(),
        }
    }

    /// The blockchain this builder is configured for.
    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Sets the Tycho API key.
    pub fn tycho_api_key(mut self, key: impl Into<String>) -> Self {
        self.tycho_api_key = Some(key.into());
        self
    }

    /// Overrides the minimum TVL filter set in [`FyndBuilder::new`].
    pub fn min_tvl(mut self, min_tvl: f64) -> Self {
        self.min_tvl = min_tvl;
        self
    }

    /// Enables or disables TLS for the Tycho WebSocket connection (default: `true`).
    pub fn tycho_use_tls(mut self, use_tls: bool) -> Self {
        self.tycho_use_tls = use_tls;
        self
    }

    /// Sets the minimum token quality score; tokens below this threshold are excluded (default:
    /// 100).
    pub fn min_token_quality(mut self, quality: i32) -> Self {
        self.min_token_quality = quality;
        self
    }

    /// Filters out pools whose last trade is older than `days` days (default: 3).
    pub fn traded_n_days_ago(mut self, days: u64) -> Self {
        self.traded_n_days_ago = days;
        self
    }

    /// Multiplies reported TVL by `ratio` before applying the `min_tvl` filter (default: 1.1).
    pub fn tvl_buffer_ratio(mut self, ratio: f64) -> Self {
        self.tvl_buffer_ratio = ratio;
        self
    }

    /// Sets how often the gas price is refreshed from the RPC node (default: 30 s).
    pub fn gas_refresh_interval(mut self, interval: Duration) -> Self {
        self.gas_refresh_interval = interval;
        self
    }

    /// Sets the delay before reconnecting to Tycho after a disconnection (default: 5 s).
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// Sets component IDs to exclude from the Tycho stream.
    pub fn blocklisted_components(mut self, components: HashSet<String>) -> Self {
        self.blocklisted_components = components;
        self
    }

    /// Enables partial block (flashblock) updates from the Tycho stream (default: `false`).
    ///
    /// When enabled, the stream delivers pool state updates mid-block rather than only at
    /// finalization, reducing latency. Only supported for on-chain protocols; RFQ streams are
    /// unaffected.
    pub fn partial_blocks(mut self, enabled: bool) -> Self {
        self.partial_blocks = enabled;
        self
    }

    /// Sets the worker router timeout (default: 10s).
    pub fn worker_router_timeout(mut self, timeout: Duration) -> Self {
        self.router_timeout = timeout;
        self
    }

    /// Sets the minimum number of solver responses before early return (default: 0).
    pub fn worker_router_min_responses(mut self, min: usize) -> Self {
        self.router_min_responses = min;
        self
    }

    /// Overrides the default encoder.
    pub fn encoder(mut self, encoder: Encoder) -> Self {
        self.encoder = Some(encoder);
        self
    }

    /// Shorthand: adds a single pool named `"default"` using a built-in algorithm by name.
    pub fn algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.pools.push(PoolEntry::BuiltIn {
            name: "default".to_string(),
            algorithm: algorithm.into(),
            num_workers: num_cpus::get(),
            task_queue_capacity: defaults::POOL_TASK_QUEUE_CAPACITY,
            min_hops: defaults::POOL_MIN_HOPS,
            max_hops: defaults::POOL_MAX_HOPS,
            timeout_ms: defaults::POOL_TIMEOUT_MS,
            max_routes: None,
            connector_tokens: None,
        });
        self
    }

    /// Shorthand: adds a single pool with a custom [`Algorithm`] implementation.
    ///
    /// The `factory` closure is called once per worker thread.
    pub fn with_algorithm<A, F>(mut self, name: impl Into<String>, factory: F) -> Self
    where
        A: Algorithm + 'static,
        A::GraphManager: MarketEventHandler + EdgeWeightUpdaterWithDerived + 'static,
        F: Fn(AlgorithmConfig) -> A + Clone + Send + Sync + 'static,
    {
        let name = name.into();
        let algo_name = name.clone();
        let configure =
            Box::new(move |builder: WorkerPoolBuilder| builder.with_algorithm(algo_name, factory));
        self.pools
            .push(PoolEntry::Custom(CustomPoolEntry {
                name,
                num_workers: num_cpus::get(),
                task_queue_capacity: defaults::POOL_TASK_QUEUE_CAPACITY,
                min_hops: defaults::POOL_MIN_HOPS,
                max_hops: defaults::POOL_MAX_HOPS,
                timeout_ms: defaults::POOL_TIMEOUT_MS,
                max_routes: None,
                configure,
            }));
        self
    }

    /// Registers the built-in price providers (Hyperliquid + Binance).
    ///
    /// Called automatically during [`build`](Self::build) if no providers have been
    /// registered and the price guard is not disabled. To use only custom
    /// providers, call [`register_price_provider`](Self::register_price_provider)
    /// before `build()` and the defaults will be skipped.
    pub fn add_default_price_providers(self) -> Self {
        self.register_price_provider(Box::new(
            crate::price_guard::hyperliquid::HyperliquidProvider::default(),
        ))
        .register_price_provider(Box::new(
            crate::price_guard::binance_ws::BinanceWsProvider::default(),
        ))
    }

    /// Registers a custom price provider for the price guard.
    ///
    /// The provider's [`start`](PriceProvider::start) method is called during
    /// [`build`](Self::build) with the shared market data.
    pub fn register_price_provider(mut self, provider: Box<dyn PriceProvider>) -> Self {
        self.price_providers.push(provider);
        self
    }

    /// Registers a [`TxDeltaIndexer`] for ephemeral pending-block simulation.
    ///
    /// `extractor` is the protocol synchronizer name (e.g. `"uniswap_v3"`). Only has effect
    /// when calling [`build_with_pending`](Self::build_with_pending). VM protocols (prefix
    /// `"vm:"`) are rejected by the underlying stream builder at build time.
    pub fn with_pending_indexer(
        mut self,
        extractor: impl Into<String>,
        indexer: Box<dyn TxDeltaIndexer>,
    ) -> Self {
        self.pending_indexers
            .push((extractor.into(), indexer));
        self
    }

    /// Enables or disables the price guard.
    ///
    /// When enabled, providers are started and caches stay warm. Validation
    /// only runs for requests where the client sets `enabled: true` in
    /// `PriceGuardConfig`. When disabled, no providers are started and
    /// per-request attempts to use the guard return an error.
    pub fn price_guard_enabled(mut self, enabled: bool) -> Self {
        self.price_guard_enabled = enabled;
        self
    }

    /// Adds a named pool using the given [`PoolConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`SolverBuildError::AlgorithmConfig`] if any address in `connector_tokens` is not
    /// valid hex.
    pub fn add_pool(
        mut self,
        name: impl Into<String>,
        config: &PoolConfig,
    ) -> Result<Self, SolverBuildError> {
        let connector_tokens = parse_connector_tokens(config.connector_tokens())?;
        self.pools.push(PoolEntry::BuiltIn {
            name: name.into(),
            algorithm: config.algorithm().to_string(),
            num_workers: config.num_workers(),
            task_queue_capacity: config.task_queue_capacity(),
            min_hops: config.min_hops(),
            max_hops: config.max_hops(),
            timeout_ms: config.timeout_ms(),
            max_routes: config.max_routes(),
            connector_tokens,
        });
        Ok(self)
    }

    /// Constructs all components shared between [`build`](Self::build) and
    /// [`build_with_pending`](Self::build_with_pending).
    fn assemble_components(mut self) -> Result<BuiltComponents, SolverBuildError> {
        if self.pools.is_empty() {
            return Err(SolverBuildError::NoPools);
        }

        // Add built-in providers if none were explicitly registered.
        if self.price_providers.is_empty() {
            self = self.add_default_price_providers();
        }

        let market_data = MarketData::new_shared();

        let tycho_feed_config = TychoFeedConfig::new(
            self.tycho_url,
            self.chain,
            self.tycho_api_key,
            self.tycho_use_tls,
            self.protocols,
            self.min_tvl,
        )
        .tvl_buffer_ratio(self.tvl_buffer_ratio)
        .reconnect_delay(self.reconnect_delay)
        .min_token_quality(self.min_token_quality)
        .traded_n_days_ago(self.traded_n_days_ago)
        .blocklisted_components(self.blocklisted_components)
        .partial_blocks(self.partial_blocks);

        let ethereum_client = EthereumRpcClient::new(self.rpc_url.as_str())
            .map_err(|e| SolverBuildError::RpcClient(e.to_string()))?;

        let gas_price_fetcher =
            GasPriceFetcher::new(ethereum_client, market_data.clone(), self.gas_refresh_interval);

        let tycho_feed = TychoFeed::new(tycho_feed_config, market_data.clone());
        let market_event_tx = tycho_feed.event_sender();

        let gas_token = native_token(&self.chain).map_err(|_| SolverBuildError::GasToken)?;
        let computation_config = ComputationManagerConfig::new()
            .with_gas_token(gas_token)
            .with_depth_slippage_threshold(DEFAULT_DEPTH_SLIPPAGE_THRESHOLD);
        // ComputationManager::new returns a broadcast receiver that we don't need here —
        // workers subscribe via computation_manager.event_sender() below.
        let (computation_manager, _) =
            ComputationManager::new(computation_config, market_data.clone())
                .map_err(|e| SolverBuildError::ComputationManager(e.to_string()))?;

        let derived_data: SharedDerivedDataRef = computation_manager.store();
        let derived_event_tx = computation_manager.event_sender();

        // Subscribe event channels before spawning (one for computation manager + one per pool)
        let computation_event_rx = tycho_feed.subscribe();
        let (computation_shutdown_tx, computation_shutdown_rx) = broadcast::channel(1);

        let mut solver_pool_handles: Vec<SolverPoolHandle> = Vec::new();
        let mut worker_pools: Vec<WorkerPool> = Vec::new();

        for pool_entry in self.pools {
            let pool_event_rx = tycho_feed.subscribe();
            let derived_rx = derived_event_tx.subscribe();

            let (worker_pool, task_handle) = match pool_entry {
                PoolEntry::BuiltIn {
                    name,
                    algorithm,
                    num_workers,
                    task_queue_capacity,
                    min_hops,
                    max_hops,
                    timeout_ms,
                    max_routes,
                    connector_tokens,
                } => {
                    let mut algo_cfg = AlgorithmConfig::new(
                        min_hops,
                        max_hops,
                        Duration::from_millis(timeout_ms),
                        max_routes,
                    )?;
                    if let Some(tokens) = connector_tokens {
                        algo_cfg = algo_cfg.with_connector_tokens(tokens);
                    }
                    WorkerPoolBuilder::new()
                        .name(name)
                        .algorithm(algorithm)
                        .algorithm_config(algo_cfg)
                        .num_workers(num_workers)
                        .task_queue_capacity(task_queue_capacity)
                        .build(
                            market_data.clone(),
                            Arc::clone(&derived_data),
                            pool_event_rx,
                            derived_rx,
                        )?
                }
                PoolEntry::Custom(custom) => {
                    let algo_cfg = AlgorithmConfig::new(
                        custom.min_hops,
                        custom.max_hops,
                        Duration::from_millis(custom.timeout_ms),
                        custom.max_routes,
                    )?;
                    let builder = WorkerPoolBuilder::new()
                        .name(custom.name)
                        .algorithm_config(algo_cfg)
                        .num_workers(custom.num_workers)
                        .task_queue_capacity(custom.task_queue_capacity);
                    let builder = (custom.configure)(builder);
                    builder.build(
                        market_data.clone(),
                        Arc::clone(&derived_data),
                        pool_event_rx,
                        derived_rx,
                    )?
                }
            };

            solver_pool_handles.push(SolverPoolHandle::new(worker_pool.name(), task_handle));
            worker_pools.push(worker_pool);
        }

        let encoder = match self.encoder {
            Some(enc) => enc,
            None => {
                let registry = SwapEncoderRegistry::new(self.chain)
                    .add_default_encoders(None)
                    .map_err(|e| SolverBuildError::Encoder(e.to_string()))?;
                Encoder::new(self.chain, registry)
                    .map_err(|e| SolverBuildError::Encoder(e.to_string()))?
            }
        };

        let chain = self.chain;
        let router_address = encoder.router_address().clone();
        let router_fees = encoder.router_fees();

        let router_fee_fetcher = RouterFeeFetcher::new(
            self.rpc_url.as_str(),
            &router_address,
            router_fees.clone(),
            defaults::ROUTER_FEE_REFRESH_INTERVAL,
        )
        .map_err(|e| SolverBuildError::RouterFeeFetcher(e.to_string()))?;

        // Only start price providers when the guard is enabled.
        // When disabled, per-request attempts to enable the guard return an error.
        let router_config = WorkerPoolRouterConfig::default()
            .with_timeout(self.router_timeout)
            .with_min_responses(self.router_min_responses);
        let mut router = WorkerPoolRouter::new(solver_pool_handles, router_config, encoder);

        if self.price_guard_enabled {
            let mut registry = PriceProviderRegistry::new();
            let mut worker_handles = Vec::new();
            for mut provider in self.price_providers {
                worker_handles.push(provider.start(market_data.clone()));
                registry = registry.register(provider);
            }
            let price_guard = PriceGuard::new(registry, worker_handles);
            router = router.with_price_guard(price_guard);
        }

        Ok(BuiltComponents {
            tycho_feed,
            gas_price_fetcher,
            router_fee_fetcher,
            computation_manager,
            computation_event_rx,
            computation_shutdown_tx,
            computation_shutdown_rx,
            router,
            worker_pools,
            market_data,
            derived_data,
            router_fees,
            chain,
            router_address,
            pending_indexers: self.pending_indexers,
            market_event_tx,
        })
    }

    /// Assembles and starts all solver components.
    ///
    /// # Errors
    ///
    /// Returns [`SolverBuildError`] if any component fails to initialize.
    pub fn build(self) -> Result<Solver, SolverBuildError> {
        let mut c = self.assemble_components()?;

        let feed_handle = tokio::spawn(async move {
            if let Err(e) = c.tycho_feed.run().await {
                tracing::error!(error = %e, "tycho feed error");
            }
        });
        let gas_price_handle = tokio::spawn(async move {
            c.gas_price_fetcher.run().await;
        });
        let router_fee_handle = tokio::spawn(async move {
            c.router_fee_fetcher.run().await;
        });
        let computation_handle = tokio::spawn(async move {
            c.computation_manager
                .run(c.computation_event_rx, c.computation_shutdown_rx)
                .await;
        });

        Ok(Solver {
            router: c.router,
            worker_pools: c.worker_pools,
            market_data: c.market_data,
            derived_data: c.derived_data,
            router_fees: c.router_fees,
            feed_handle,
            gas_price_handle,
            router_fee_handle,
            computation_handle,
            computation_shutdown_tx: c.computation_shutdown_tx,
            chain: c.chain,
            router_address: c.router_address,
            market_event_tx: c.market_event_tx,
        })
    }

    /// Assembles and starts all solver components, also returning a [`PendingBlockProcessor`]
    /// for ephemeral bundle simulation against the live Tycho market state.
    ///
    /// Identical to [`build`](Self::build) except the feed task runs via
    /// `TychoFeed::run_with_pending`. The `PendingBlockProcessor` is delivered after
    /// token loading, before the first block is processed.
    ///
    /// # Errors
    ///
    /// Returns [`SolverBuildError`] if any component fails to initialize or the pending
    /// channel closes before delivering the processor (e.g. token loading failed).
    pub async fn build_with_pending(
        self,
    ) -> Result<(Solver, PendingBlockProcessor), SolverBuildError> {
        let mut c = self.assemble_components()?;

        let (pending_tx, pending_rx) =
            tokio::sync::oneshot::channel::<Result<PendingBlockProcessor, String>>();

        let pending_indexers = c.pending_indexers;
        let feed_handle = tokio::spawn(async move {
            if let Err(e) = c
                .tycho_feed
                .run_with_pending(pending_tx, pending_indexers)
                .await
            {
                tracing::error!(error = %e, "tycho feed error");
            }
        });
        let gas_price_handle = tokio::spawn(async move {
            c.gas_price_fetcher.run().await;
        });
        let router_fee_handle = tokio::spawn(async move {
            c.router_fee_fetcher.run().await;
        });
        let computation_handle = tokio::spawn(async move {
            c.computation_manager
                .run(c.computation_event_rx, c.computation_shutdown_rx)
                .await;
        });

        let pending = pending_rx
            .await
            .map_err(|_| SolverBuildError::PendingChannelClosed)?
            .map_err(SolverBuildError::FeedSetup)?;

        Ok((
            Solver {
                router: c.router,
                worker_pools: c.worker_pools,
                market_data: c.market_data,
                derived_data: c.derived_data,
                router_fees: c.router_fees,
                feed_handle,
                gas_price_handle,
                router_fee_handle,
                computation_handle,
                computation_shutdown_tx: c.computation_shutdown_tx,
                chain: c.chain,
                router_address: c.router_address,
                market_event_tx: c.market_event_tx,
            },
            pending,
        ))
    }
}

/// A running solver assembled by [`FyndBuilder`].
pub struct Solver {
    router: WorkerPoolRouter,
    worker_pools: Vec<WorkerPool>,
    market_data: MarketData,
    derived_data: SharedDerivedDataRef,
    router_fees: SharedRouterFees,
    feed_handle: JoinHandle<()>,
    gas_price_handle: JoinHandle<()>,
    router_fee_handle: JoinHandle<()>,
    computation_handle: JoinHandle<()>,
    computation_shutdown_tx: broadcast::Sender<()>,
    chain: Chain,
    router_address: Bytes,
    market_event_tx: broadcast::Sender<MarketEvent>,
}

impl Solver {
    /// Returns a clone of the shared market data reference.
    pub fn market_data(&self) -> MarketData {
        self.market_data.clone()
    }

    /// Returns a clone of the shared derived data reference.
    pub fn derived_data(&self) -> SharedDerivedDataRef {
        Arc::clone(&self.derived_data)
    }

    /// Returns a new receiver for [`MarketEvent`]s broadcast by the Tycho feed.
    ///
    /// Each call returns an independent receiver. Events are broadcast on every block update.
    /// Receivers created after a block has been processed will miss that block's event.
    pub fn subscribe_market_events(&self) -> broadcast::Receiver<crate::feed::events::MarketEvent> {
        self.market_event_tx.subscribe()
    }

    /// Submits a [`QuoteRequest`] to the worker pools and returns the best [`Quote`].
    ///
    /// # Errors
    ///
    /// Returns [`SolveError`] if all pools fail or the router timeout elapses.
    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, SolveError> {
        self.router.quote(request).await
    }

    /// Waits until the solver is ready to answer quotes.
    ///
    /// Ready means:
    /// - The Tycho feed has delivered at least one market snapshot.
    /// - The computation manager has completed at least one derived-data cycle (spot prices, pool
    ///   depths, token gas prices).
    /// - Router fees have been loaded from the on-chain FeeCalculator at least once.
    ///
    /// The method polls every 500 ms and returns as soon as all conditions are
    /// met, or returns [`WaitReadyError`] if `timeout` elapses first.
    ///
    /// # Example
    ///
    /// ```ignore
    /// solver.wait_until_ready(Duration::from_secs(180)).await?;
    /// ```
    pub async fn wait_until_ready(&self, timeout: Duration) -> Result<(), WaitReadyError> {
        const POLL_INTERVAL: Duration = Duration::from_millis(500);

        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let market_ready = self
                .market_data
                .read()
                .await
                .last_updated()
                .is_some();
            let derived_ready = self
                .derived_data
                .read()
                .await
                .derived_data_ready();

            if market_ready && derived_ready {
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(WaitReadyError { timeout_ms: timeout.as_millis() as u64 });
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Build a Solver by replaying recorded market updates.
    ///
    /// Creates the full pipeline (feed -> derived data -> worker pools -> router)
    /// from pre-recorded data instead of a live Tycho connection. The returned
    /// Solver behaves identically to a live one — call [`wait_until_ready`](Self::wait_until_ready)
    /// then [`quote`](Self::quote).
    ///
    /// VM-backed protocol states that couldn't be serialized will be absent from
    /// the recording. Pools without states will be registered as components but
    /// won't contribute to routing.
    ///
    /// Requires the `test-utils` feature.
    #[cfg(feature = "test-utils")]
    pub async fn from_recording(
        chain: Chain,
        updates: Vec<tycho_simulation::protocol::models::Update>,
        pools: std::collections::HashMap<String, PoolConfig>,
        gas_price_wei: Option<num_bigint::BigUint>,
    ) -> Result<Self, SolverBuildError> {
        if pools.is_empty() {
            return Err(SolverBuildError::NoPools);
        }

        let market_data = MarketData::new_shared();

        // Replay updates through TychoFeed (stays pub(crate))
        let feed_config =
            TychoFeedConfig::new("ws://replay".to_string(), chain, None, false, vec![], 0.0);
        let feed = TychoFeed::new(feed_config, market_data.clone());
        let market_event_tx = feed.event_sender();
        let _feed_rx = feed.subscribe();

        for update in updates {
            feed.handle_tycho_message(update)
                .await
                .map_err(|e| SolverBuildError::Replay(e.to_string()))?;
        }

        // Inject gas price (recorded value or default 10 gwei)
        let gas_price = match gas_price_wei {
            Some(price) => price,
            None => {
                tracing::warn!("no recorded gas price, defaulting to 10 gwei");
                num_bigint::BigUint::from(10_000_000_000u64)
            }
        };
        let block_number = match market_data.read().await.last_updated() {
            Some(block) => block.number(),
            None => {
                tracing::warn!("no block number from replayed updates, defaulting to 0");
                0
            }
        };
        {
            let mut market = market_data.write().await;
            market.update_gas_price(BlockGasPrice {
                block_number,
                block_hash: Default::default(),
                block_timestamp: 0,
                pricing: GasPrice::Legacy { gas_price },
            });
        }

        // Computation manager
        let gas_token = native_token(&chain).map_err(|_| SolverBuildError::GasToken)?;
        let computation_config = ComputationManagerConfig::new()
            .with_gas_token(gas_token)
            .with_depth_slippage_threshold(DEFAULT_DEPTH_SLIPPAGE_THRESHOLD);
        let (computation_manager, _) =
            ComputationManager::new(computation_config, market_data.clone())
                .map_err(|e| SolverBuildError::ComputationManager(e.to_string()))?;

        let derived_data: SharedDerivedDataRef = computation_manager.store();
        let derived_event_tx = computation_manager.event_sender();

        let computation_event_rx = feed.subscribe();
        let (computation_shutdown_tx, computation_shutdown_rx) = broadcast::channel(1);

        let computation_handle = tokio::spawn(async move {
            computation_manager
                .run(computation_event_rx, computation_shutdown_rx)
                .await;
        });

        // Build worker pools BEFORE sending MarketUpdated
        let mut solver_pool_handles: Vec<SolverPoolHandle> = Vec::new();
        let mut worker_pools: Vec<WorkerPool> = Vec::new();
        let mut max_timeout_ms = 0u64;

        for (name, pool_cfg) in &pools {
            let algo_cfg = AlgorithmConfig::new(
                pool_cfg.min_hops(),
                pool_cfg.max_hops(),
                Duration::from_millis(pool_cfg.timeout_ms()),
                pool_cfg.max_routes(),
            )?;

            let pool_event_rx = feed.subscribe();
            let derived_rx = derived_event_tx.subscribe();

            let (worker_pool, task_handle) = WorkerPoolBuilder::new()
                .name(name.clone())
                .algorithm(pool_cfg.algorithm().to_string())
                .algorithm_config(algo_cfg)
                .num_workers(pool_cfg.num_workers())
                .task_queue_capacity(pool_cfg.task_queue_capacity())
                .build(market_data.clone(), Arc::clone(&derived_data), pool_event_rx, derived_rx)?;

            solver_pool_handles.push(SolverPoolHandle::new(worker_pool.name(), task_handle));
            max_timeout_ms = max_timeout_ms.max(pool_cfg.timeout_ms());
            worker_pools.push(worker_pool);
        }

        // Encoder + router
        let encoder = {
            let registry = SwapEncoderRegistry::new(chain)
                .add_default_encoders(None)
                .map_err(|e| SolverBuildError::Encoder(e.to_string()))?;
            Encoder::new(chain, registry).map_err(|e| SolverBuildError::Encoder(e.to_string()))?
        };

        let router_address = encoder.router_address().clone();
        // Replay mode has no FeeCalculator to read; seed a zero-fee config at the standard
        // 8-decimal scale so the recording-based solver reports ready (integration tests do
        // not exercise encoding).
        let router_fees = encoder.router_fees();
        router_fees.set(crate::encoding::router_fees::RouterFees::new(
            100_000_000,
            0,
            0,
            std::collections::HashMap::new(),
        ));
        let router_config = WorkerPoolRouterConfig::default()
            .with_timeout(Duration::from_millis(max_timeout_ms.max(5000)))
            .with_min_responses(defaults::ROUTER_MIN_RESPONSES);
        let router = WorkerPoolRouter::new(solver_pool_handles, router_config, encoder);

        // Trigger derived data computation
        let market_read = market_data.read().await;
        let added = market_read.component_topology();
        drop(market_read);

        if market_event_tx
            .send(MarketEvent::MarketUpdated {
                added_components: added,
                removed_components: vec![],
                updated_components: vec![],
            })
            .is_err()
        {
            tracing::warn!("no receivers for initial MarketUpdated broadcast");
        }

        // Dummy handles for feed/gas/router-fees (not running in replay mode). The market
        // event channel stays alive through the `market_event_tx` field on `Solver`.
        let feed_handle = tokio::spawn(futures::future::pending::<()>());
        let gas_price_handle = tokio::spawn(async { /* no-op */ });
        let router_fee_handle = tokio::spawn(async { /* no-op */ });

        Ok(Solver {
            router,
            worker_pools,
            market_data,
            derived_data,
            router_fees,
            feed_handle,
            gas_price_handle,
            router_fee_handle,
            computation_handle,
            computation_shutdown_tx,
            chain,
            router_address,
            market_event_tx,
        })
    }

    /// Signals all worker pools and the computation manager to stop, then aborts background tasks.
    pub fn shutdown(self) {
        let _ = self.computation_shutdown_tx.send(());
        for pool in self.worker_pools {
            pool.shutdown();
        }
        self.feed_handle.abort();
        self.gas_price_handle.abort();
        self.router_fee_handle.abort();
    }

    /// Consumes the solver into its raw parts for callers that add their own layer.
    pub fn into_parts(self) -> SolverParts {
        SolverParts {
            router: self.router,
            worker_pools: self.worker_pools,
            market_data: self.market_data,
            derived_data: self.derived_data,
            router_fees: self.router_fees,
            feed_handle: self.feed_handle,
            gas_price_handle: self.gas_price_handle,
            router_fee_handle: self.router_fee_handle,
            computation_handle: self.computation_handle,
            computation_shutdown_tx: self.computation_shutdown_tx,
            chain: self.chain,
            router_address: self.router_address,
        }
    }
}

/// Raw components of a [`Solver`], for callers adding their own layer (e.g., an HTTP server).
///
/// Obtained via [`Solver::into_parts`].
pub struct SolverParts {
    /// Routes quote requests across worker pools.
    router: WorkerPoolRouter,
    /// One [`WorkerPool`] per configured algorithm pool.
    worker_pools: Vec<WorkerPool>,
    /// Live market snapshot shared across all components.
    market_data: MarketData,
    /// Derived on-chain data (spot prices, depths, gas costs) shared across all components.
    derived_data: SharedDerivedDataRef,
    /// Router fee configuration, refreshed from chain by the router-fee fetcher.
    router_fees: SharedRouterFees,
    /// Background task running the Tycho market-data feed.
    feed_handle: JoinHandle<()>,
    /// Background task polling the RPC node for gas prices.
    gas_price_handle: JoinHandle<()>,
    /// Background task refreshing router fees from the on-chain FeeCalculator.
    router_fee_handle: JoinHandle<()>,
    /// Background task running the computation manager.
    computation_handle: JoinHandle<()>,
    /// Send a unit value on this channel to trigger a graceful computation-manager shutdown.
    computation_shutdown_tx: broadcast::Sender<()>,
    /// Chain this solver is configured for.
    chain: Chain,
    /// Address of the Tycho Router contract on this chain.
    router_address: Bytes,
}

impl SolverParts {
    /// Returns the chain this solver is configured for.
    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Returns the Tycho Router contract address for this chain.
    pub fn router_address(&self) -> &Bytes {
        &self.router_address
    }

    /// Returns a reference to the worker pools.
    pub fn worker_pools(&self) -> &[WorkerPool] {
        &self.worker_pools
    }

    /// Returns a reference to the shared market data.
    pub fn market_data(&self) -> &MarketData {
        &self.market_data
    }

    /// Returns a reference to the shared derived data.
    pub fn derived_data(&self) -> &SharedDerivedDataRef {
        &self.derived_data
    }

    /// Returns a reference to the shared router fee configuration.
    pub fn router_fees(&self) -> &SharedRouterFees {
        &self.router_fees
    }

    /// Consumes the parts and returns the router.
    pub fn into_router(self) -> WorkerPoolRouter {
        self.router
    }

    /// Consumes the parts, returning all owned components.
    #[allow(clippy::type_complexity)]
    pub fn into_components(
        self,
    ) -> (
        WorkerPoolRouter,
        Vec<WorkerPool>,
        MarketData,
        SharedDerivedDataRef,
        JoinHandle<()>,
        JoinHandle<()>,
        JoinHandle<()>,
        JoinHandle<()>,
        broadcast::Sender<()>,
    ) {
        (
            self.router,
            self.worker_pools,
            self.market_data,
            self.derived_data,
            self.feed_handle,
            self.gas_price_handle,
            self.router_fee_handle,
            self.computation_handle,
            self.computation_shutdown_tx,
        )
    }
}
