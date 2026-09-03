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
use std::{str::FromStr, sync::Arc, time::Duration};

use num_cpus;
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast, task::JoinHandle};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
#[cfg(feature = "experimental")]
use tycho_simulation::evm::stream::BlockStepController;
#[cfg(feature = "test-utils")]
use tycho_simulation::tycho_ethereum::gas::{BlockGasPrice, GasPrice};
use tycho_simulation::{
    evm::pending::PendingBlockProcessor,
    tycho_common::{models::Chain, traits::TxDeltaIndexer, Bytes},
    tycho_core::models::Address,
    tycho_ethereum::rpc::EthereumRpcClient,
};

use crate::{
    algorithm::{AlgorithmConfig, AlgorithmError, AlgorithmRegistry},
    derived::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef},
    encoding::{encoder::Encoder, fee_fetcher::RouterFeeFetcher, router_fees::SharedRouterFees},
    feed::{
        events::{MarketEvent, MarketEventHandler},
        gas::GasPriceFetcher,
        market_data::MarketData,
        metrics_sampler::MetricsSampler,
        tycho_feed::TychoFeed,
        TychoFeedConfig,
    },
    graph::EdgeWeightUpdaterWithDerived,
    price_guard::{
        guard::PriceGuard, provider::PriceProvider, provider_registry::PriceProviderRegistry,
    },
    simulation::simulator::QuoteSimulator,
    types::constants::native_token,
    worker_pool::{
        pool::{WorkerPool, WorkerPoolBuilder},
        registry::UnknownAlgorithmError,
    },
    worker_pool_router::{
        config::WorkerPoolRouterConfig, ExclusiveAccess, LiquidityScope, SolverPoolHandle,
        WorkerPoolRouter,
    },
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
    /// Multiplier applied to a component's (liquidity pool's) TVL when estimating available
    /// liquidity.
    pub const TVL_BUFFER_RATIO: f64 = 1.1;
    /// How often the gas price is refreshed from the RPC node.
    pub const GAS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
    /// How often per-protocol market metrics are sampled and exported.
    pub const METRICS_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
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
    /// Per-worker-pool solve timeout in milliseconds.
    pub const POOL_TIMEOUT_MS: u64 = 100;
    /// Limits each simulation RPC request so optional quote simulation cannot delay quotes.
    pub const SIMULATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
    /// Limits slot discovery independently, leaving a full request budget for the simulation call.
    pub const SIMULATION_SLOT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
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
) -> Result<Option<FxHashSet<Address>>, SolverBuildError> {
    let Some(strings) = raw else {
        return Ok(None);
    };
    let mut set = FxHashSet::with_capacity_and_hasher(strings.len(), Default::default());
    for s in strings {
        let addr = Address::from_str(s).map_err(|e| AlgorithmError::InvalidConfiguration {
            reason: format!("connector_tokens: invalid address {s:?}: {e}"),
        })?;
        set.insert(addr);
    }
    Ok(Some(set))
}

/// Configuration for one worker pool, used by [`FyndBuilder::add_pool`].
#[must_use]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Algorithm name for this worker pool (e.g., `"most_liquid"`).
    algorithm: String,
    /// Number of worker threads for this worker pool.
    #[serde(default = "num_cpus::get")]
    num_workers: usize,
    /// Task queue capacity for this worker pool.
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
    /// Which liquidity this worker pool routes through.
    #[serde(default)]
    liquidity_scope: Option<LiquidityScope>,
    /// Protocol systems this worker pool's workers never route through, e.g.
    /// `["propammfallback:"]`. Absent = no restriction.
    #[serde(default)]
    exclude_protocols: Option<Vec<String>>,
}

impl PoolConfig {
    /// Creates a new worker pool config with the given algorithm name and defaults for all other
    /// fields.
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
            liquidity_scope: None,
            exclude_protocols: None,
        }
    }

    /// Returns the algorithm name.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the worker pool's liquidity scope.
    pub fn liquidity_scope(&self) -> Option<LiquidityScope> {
        self.liquidity_scope
    }

    /// Sets the worker pool's liquidity scope.
    pub fn with_liquidity_scope(mut self, scope: LiquidityScope) -> Self {
        self.liquidity_scope = Some(scope);
        self
    }

    /// Returns the protocol systems this worker pool never routes through.
    pub fn exclude_protocols(&self) -> Option<&[String]> {
        self.exclude_protocols.as_deref()
    }

    /// Sets the protocol systems this worker pool never routes through. An entry names a protocol
    /// system exactly (`"uniswap_v2"`), or the whole family under a prefix when it ends with `:`
    /// (`"propammfallback:"` covers every venue on the PropAMMRouter).
    pub fn with_exclude_protocols(mut self, exclude_protocols: Vec<String>) -> Self {
        self.exclude_protocols = Some(exclude_protocols);
        self
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
    /// The quote simulator could not be created (e.g. malformed RPC URL).
    #[error("failed to create quote simulator: {0}")]
    QuoteSimulator(String),
    /// A worker pool referenced an algorithm name that is not registered.
    #[error(transparent)]
    UnknownAlgorithm(#[from] UnknownAlgorithmError),
    /// No native gas token is defined for the requested chain.
    #[error("gas token not configured for chain")]
    GasToken,
    /// [`FyndBuilder::build`] was called without configuring any worker pools.
    #[error("no worker pools configured")]
    NoPools,
    /// Every worker pool is `liquidity_scope = "include_exclusive"`, so a request without the
    /// exclusive access would be served by no worker pool at all. At least one worker
    /// pool must route public liquidity.
    #[error(
        "every worker pool sets liquidity_scope = \"include_exclusive\"; requests without \
         exclusive access would be served by no pool. Configure at least one public_only pool"
    )]
    NoPublicPool,
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
    /// The step-controller oneshot closed without delivering a value, meaning the feed task
    /// panicked rather than returning an error through the channel.
    #[cfg(feature = "experimental")]
    #[error("step controller channel closed before controller was delivered")]
    StepControllerChannelClosed,
}

/// Internal worker pool entry — either a built-in algorithm (by name) or a custom one.
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
        connector_tokens: Option<FxHashSet<Address>>,
        liquidity_scope: Option<LiquidityScope>,
        exclude_protocols: Vec<String>,
    },
    Custom(CustomPoolEntry),
}

impl PoolEntry {
    /// Returns the configured liquidity scope for this worker pool.
    fn liquidity_scope(&self) -> Option<LiquidityScope> {
        match self {
            PoolEntry::BuiltIn { liquidity_scope, .. } => *liquidity_scope,
            PoolEntry::Custom(custom) => custom.liquidity_scope,
        }
    }
}

/// Worker pool entry backed by a custom [`Algorithm`] implementation.
struct CustomPoolEntry {
    name: String,
    num_workers: usize,
    task_queue_capacity: usize,
    min_hops: usize,
    max_hops: usize,
    timeout_ms: u64,
    max_routes: Option<usize>,
    liquidity_scope: Option<LiquidityScope>,
    /// Applies the custom algorithm to a `WorkerPoolBuilder`.
    configure: Box<dyn FnOnce(WorkerPoolBuilder) -> WorkerPoolBuilder + Send>,
}

/// All components produced by [`FyndBuilder::assemble_components`], consumed by
/// [`FyndBuilder::build`] and [`FyndBuilder::build_with_pending`].
struct BuiltComponents {
    tycho_feed: TychoFeed,
    gas_price_fetcher: GasPriceFetcher<EthereumRpcClient>,
    router_fee_fetcher: Option<RouterFeeFetcher>,
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
    router_address: Option<Bytes>,
    pending_indexers: Vec<(String, Box<dyn TxDeltaIndexer>)>,
    market_event_tx: broadcast::Sender<MarketEvent>,
}

/// Builder for assembling the full solver pipeline.
///
/// Configures the Tycho market-data feed, gas price fetcher, derived-data
/// computation manager, one or more worker pools, encoder, and router.
#[must_use = "a builder does nothing until .build() is called"]
pub struct FyndBuilder {
    /// Algorithms the caller brought, served when a pool names one.
    algorithms: AlgorithmRegistry,
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
    blocklisted_components: FxHashSet<String>,
    partial_blocks: bool,
    router_timeout: Duration,
    router_min_responses: usize,
    encoder: Option<Encoder>,
    calldata_watermark: Option<Vec<u8>>,
    pools: Vec<PoolEntry>,
    price_guard_enabled: bool,
    simulation_enabled: bool,
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
            algorithms: AlgorithmRegistry::new(),
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
            blocklisted_components: FxHashSet::default(),
            partial_blocks: false,
            router_timeout: DEFAULT_ROUTER_TIMEOUT,
            router_min_responses: defaults::ROUTER_MIN_RESPONSES,
            encoder: None,
            calldata_watermark: None,
            pools: Vec::new(),
            price_guard_enabled: false,
            simulation_enabled: false,
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

    /// Filters out components whose last trade is older than `days` days (default: 3).
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
    pub fn blocklisted_components(mut self, components: impl IntoIterator<Item = String>) -> Self {
        self.blocklisted_components = components.into_iter().collect();
        self
    }

    /// Enables partial block (flashblock) updates from the Tycho stream (default: `false`).
    ///
    /// When enabled, the stream delivers component state updates mid-block rather than only at
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

    /// Sets a watermark appended to every encoded transaction's calldata (e.g. `"fynd"`), so
    /// on-chain observers can attribute router calls to this deployment. Applied to the encoder
    /// at build time, whether default or overridden. Default: no watermark.
    pub fn calldata_watermark(mut self, watermark: impl Into<Vec<u8>>) -> Self {
        self.calldata_watermark = Some(watermark.into());
        self
    }

    /// Shorthand: adds a single worker pool named `"default"` using a built-in algorithm by name.
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
            liquidity_scope: None,
            exclude_protocols: Vec::new(),
        });
        self
    }

    /// Shorthand: adds a single worker pool with a custom [`Algorithm`] implementation.
    ///
    /// The `factory` closure is called once per worker thread.
    #[deprecated(
        since = "0.99.23",
        note = "register the algorithm in an `AlgorithmRegistry` and pass it to \
                `with_algorithms`, which also serves pools that name it in a configuration \
                file; this shorthand only ever added one pool"
    )]
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
                liquidity_scope: None,
                configure,
            }));
        self
    }

    /// Serves any pool whose configured algorithm name `algorithms` holds.
    ///
    /// A pool configuration names an algorithm; only the built-ins are known by name here. This
    /// hands the builder the ones the caller brought, so a deployment can run an algorithm that
    /// lives outside this crate without changing how its pools are configured.
    pub fn with_algorithms(mut self, algorithms: AlgorithmRegistry) -> Self {
        self.algorithms = algorithms;
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

    /// Enables or disables on-chain simulation of encoded quotes.
    ///
    /// When disabled, requests that ask for simulation return an error without making RPC calls.
    pub fn simulation_enabled(mut self, enabled: bool) -> Self {
        self.simulation_enabled = enabled;
        self
    }

    /// Adds a named worker pool using the given [`PoolConfig`].
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
            liquidity_scope: config.liquidity_scope(),
            exclude_protocols: config
                .exclude_protocols()
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
        });
        Ok(self)
    }

    /// Constructs all components shared between [`build`](Self::build) and
    /// [`build_with_pending`](Self::build_with_pending).
    fn assemble_components(mut self) -> Result<BuiltComponents, SolverBuildError> {
        if self.pools.is_empty() {
            return Err(SolverBuildError::NoPools);
        }

        // Exclusive-access worker pools only serve requests granted access, so a deployment made
        // entirely of them would allocate no worker pool at all to everyone else. Caught here
        // rather than per request: it is a configuration mistake, not a runtime condition.
        if self
            .pools
            .iter()
            .all(|p| p.liquidity_scope() == Some(LiquidityScope::IncludeExclusive))
        {
            return Err(SolverBuildError::NoPublicPool);
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

        // Subscribe event channels before spawning (one for computation manager + one per worker
        // pool)
        let computation_event_rx = tycho_feed.subscribe();
        let (computation_shutdown_tx, computation_shutdown_rx) = broadcast::channel(1);

        let mut solver_pool_handles: Vec<SolverPoolHandle> = Vec::new();
        let mut worker_pools: Vec<WorkerPool> = Vec::new();

        let pools = std::mem::take(&mut self.pools);

        for pool_entry in pools {
            let pool_event_rx = tycho_feed.subscribe();
            let derived_rx = derived_event_tx.subscribe();

            let pool_scope = pool_entry
                .liquidity_scope()
                .unwrap_or_default();

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
                    liquidity_scope: _,
                    exclude_protocols,
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
                    let named = WorkerPoolBuilder::new()
                        .name(name)
                        .algorithm_config(algo_cfg)
                        .num_workers(num_workers)
                        .task_queue_capacity(task_queue_capacity)
                        .liquidity_scope(pool_scope)
                        .exclude_protocols(exclude_protocols);
                    let builder = self
                        .algorithms
                        .configure(&algorithm, named)?;
                    builder.build(
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
                        .task_queue_capacity(custom.task_queue_capacity)
                        .liquidity_scope(pool_scope);
                    let builder = (custom.configure)(builder);
                    builder.build(
                        market_data.clone(),
                        Arc::clone(&derived_data),
                        pool_event_rx,
                        derived_rx,
                    )?
                }
            };

            solver_pool_handles.push(
                SolverPoolHandle::new(worker_pool.name(), task_handle)
                    .with_liquidity_scope(pool_scope),
            );
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
        let encoder = match self.calldata_watermark {
            Some(watermark) => encoder.with_calldata_watermark(watermark),
            None => encoder,
        };

        let chain = self.chain;
        let router_address = encoder.router_address().cloned();
        let router_fees = encoder.router_fees();

        let router_fee_fetcher = match &router_address {
            Some(addr) => Some(
                RouterFeeFetcher::new(
                    self.rpc_url.as_str(),
                    addr,
                    router_fees.clone(),
                    defaults::ROUTER_FEE_REFRESH_INTERVAL,
                )
                .map_err(|e| SolverBuildError::RouterFeeFetcher(e.to_string()))?,
            ),
            None => {
                tracing::warn!(
                    %chain,
                    "no Tycho router for this chain; running quote-only (encoding disabled)"
                );
                None
            }
        };

        let quote_simulator = if self.simulation_enabled {
            Some(
                QuoteSimulator::new(
                    self.rpc_url.as_str(),
                    chain,
                    defaults::SIMULATION_REQUEST_TIMEOUT,
                )
                .map_err(|error| SolverBuildError::QuoteSimulator(error.to_string()))?,
            )
        } else {
            None
        };

        // Only start price providers when the guard is enabled.
        // When disabled, per-request attempts to enable the guard return an error.
        let router_config = WorkerPoolRouterConfig::default()
            .with_timeout(self.router_timeout)
            .with_min_responses(self.router_min_responses);
        let mut router = WorkerPoolRouter::new(solver_pool_handles, router_config, encoder);
        if let Some(simulator) = quote_simulator {
            router = router.with_simulator(simulator);
        }

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
                metrics::counter!("tycho_feed_failures_total").increment(1);
                tracing::error!(error = %e, "tycho feed error");
            }
        });
        let gas_price_handle = tokio::spawn(async move {
            c.gas_price_fetcher.run().await;
        });
        let metrics_sampler =
            MetricsSampler::new(c.market_data.clone(), defaults::METRICS_SAMPLE_INTERVAL);
        let metrics_sampler_handle = tokio::spawn(async move { metrics_sampler.run().await });
        let router_fee_handle = match c.router_fee_fetcher {
            Some(fetcher) => tokio::spawn(async move { fetcher.run().await }),
            None => tokio::spawn(async {}),
        };
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
            metrics_sampler_handle,
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
                metrics::counter!("tycho_feed_failures_total").increment(1);
                tracing::error!(error = %e, "tycho feed error");
            }
        });
        let gas_price_handle = tokio::spawn(async move {
            c.gas_price_fetcher.run().await;
        });
        let metrics_sampler =
            MetricsSampler::new(c.market_data.clone(), defaults::METRICS_SAMPLE_INTERVAL);
        let metrics_sampler_handle = tokio::spawn(async move { metrics_sampler.run().await });
        let router_fee_handle = match c.router_fee_fetcher {
            Some(fetcher) => tokio::spawn(async move { fetcher.run().await }),
            None => tokio::spawn(async {}),
        };
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
                metrics_sampler_handle,
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

    /// Assembles and starts all solver components, also returning a [`BlockStepController`]
    /// that lets the caller control when each buffered block is released for processing.
    ///
    /// Intended for deterministic testing: call [`BlockStepController::trigger_next_block`] to
    /// step through blocks one at a time, and [`BlockStepController::peek_next_block`] to inspect
    /// a block before it is decoded. Dropping the controller ungates the stream so it runs to its
    /// natural end.
    ///
    /// Only valid when at least one non-RFQ protocol is configured.
    ///
    /// # Errors
    ///
    /// Returns [`SolverBuildError`] if any component fails to initialize, all protocols are RFQ,
    /// or the step-controller channel closes before the controller is delivered.
    #[cfg(feature = "experimental")]
    pub async fn build_with_step_controller(
        self,
    ) -> Result<(Solver, BlockStepController), SolverBuildError> {
        let mut c = self.assemble_components()?;

        let (controller_tx, controller_rx) =
            tokio::sync::oneshot::channel::<Result<BlockStepController, String>>();

        let feed_handle = tokio::spawn(async move {
            if let Err(e) = c
                .tycho_feed
                .run_with_step_controller(controller_tx)
                .await
            {
                tracing::error!(error = %e, "tycho feed error");
            }
        });
        let gas_price_handle = tokio::spawn(async move {
            c.gas_price_fetcher.run().await;
        });
        let metrics_sampler =
            MetricsSampler::new(c.market_data.clone(), defaults::METRICS_SAMPLE_INTERVAL);
        let metrics_sampler_handle = tokio::spawn(async move { metrics_sampler.run().await });
        let router_fee_handle = match c.router_fee_fetcher {
            Some(fetcher) => tokio::spawn(async move { fetcher.run().await }),
            None => tokio::spawn(async {}),
        };
        let computation_handle = tokio::spawn(async move {
            c.computation_manager
                .run(c.computation_event_rx, c.computation_shutdown_rx)
                .await;
        });

        let controller = controller_rx
            .await
            .map_err(|_| SolverBuildError::StepControllerChannelClosed)?
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
                metrics_sampler_handle,
                router_fee_handle,
                computation_handle,
                computation_shutdown_tx: c.computation_shutdown_tx,
                chain: c.chain,
                router_address: c.router_address,
                market_event_tx: c.market_event_tx,
            },
            controller,
        ))
    }
} // impl FyndBuilder

/// A running solver assembled by [`FyndBuilder`].
pub struct Solver {
    router: WorkerPoolRouter,
    worker_pools: Vec<WorkerPool>,
    market_data: MarketData,
    derived_data: SharedDerivedDataRef,
    router_fees: SharedRouterFees,
    feed_handle: JoinHandle<()>,
    gas_price_handle: JoinHandle<()>,
    metrics_sampler_handle: JoinHandle<()>,
    router_fee_handle: JoinHandle<()>,
    computation_handle: JoinHandle<()>,
    computation_shutdown_tx: broadcast::Sender<()>,
    chain: Chain,
    router_address: Option<Bytes>,
    market_event_tx: broadcast::Sender<MarketEvent>,
}

impl Solver {
    /// Returns a clone of the shared market data reference.
    pub fn market_data(&self) -> MarketData {
        self.market_data.clone()
    }

    /// Returns the Tycho Router contract address, or `None` on a quote-only chain.
    pub fn router_address(&self) -> Option<&Bytes> {
        self.router_address.as_ref()
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
    /// Grants `ExclusiveAccess::Granted`: a library embedder configures its own pools, so there
    /// is no untrusted caller to gate here. Access is decided at the HTTP boundary, where
    /// requests do come from untrusted callers.
    ///
    /// # Errors
    ///
    /// Returns [`SolveError`] if all worker pools fail or the router timeout elapses.
    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, SolveError> {
        self.router
            .quote(request, ExclusiveAccess::Granted)
            .await
    }

    /// Waits until the solver is ready to answer quotes.
    ///
    /// Ready means:
    /// - The Tycho feed has delivered at least one market snapshot.
    /// - The computation manager has completed at least one derived-data cycle (spot prices,
    ///   component depths, token gas prices).
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
    /// the recording. Components without states will still be registered but
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
        Self::from_recording_with(chain, updates, pools, gas_price_wei, &AlgorithmRegistry::new())
            .await
    }

    /// [`from_recording`](Self::from_recording), with algorithms the caller brought.
    ///
    /// A pool naming an algorithm in `algorithms` is served by it; every other pool falls back to
    /// the built-in of that name. This is what lets a benchmark or a profiler run an algorithm
    /// that lives outside this crate.
    ///
    /// # Errors
    ///
    /// The same as [`from_recording`](Self::from_recording).
    ///
    /// Requires the `test-utils` feature.
    #[cfg(feature = "test-utils")]
    pub async fn from_recording_with(
        chain: Chain,
        updates: Vec<tycho_simulation::protocol::models::Update>,
        pools: std::collections::HashMap<String, PoolConfig>,
        gas_price_wei: Option<num_bigint::BigUint>,
        algorithms: &AlgorithmRegistry,
    ) -> Result<Self, SolverBuildError> {
        if pools.is_empty() {
            return Err(SolverBuildError::NoPools);
        }
        if pools
            .values()
            .all(|pool| pool.liquidity_scope() == Some(LiquidityScope::IncludeExclusive))
        {
            return Err(SolverBuildError::NoPublicPool);
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
            let mut algo_cfg = AlgorithmConfig::new(
                pool_cfg.min_hops(),
                pool_cfg.max_hops(),
                Duration::from_millis(pool_cfg.timeout_ms()),
                pool_cfg.max_routes(),
            )?;
            if let Some(tokens) = parse_connector_tokens(pool_cfg.connector_tokens())? {
                algo_cfg = algo_cfg.with_connector_tokens(tokens);
            }

            let pool_event_rx = feed.subscribe();
            let derived_rx = derived_event_tx.subscribe();

            let named = WorkerPoolBuilder::new()
                .name(name.clone())
                .algorithm_config(algo_cfg)
                .num_workers(pool_cfg.num_workers())
                .task_queue_capacity(pool_cfg.task_queue_capacity())
                .liquidity_scope(
                    pool_cfg
                        .liquidity_scope()
                        .unwrap_or_default(),
                );
            let (worker_pool, task_handle) = algorithms
                .configure(pool_cfg.algorithm(), named)?
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

        let router_address = encoder.router_address().cloned();
        // Replay mode has no FeeCalculator to read; seed a zero-fee config at the standard
        // 8-decimal scale so the recording-based solver reports ready (integration tests do
        // not exercise encoding).
        let router_fees = encoder.router_fees();
        router_fees.set(crate::encoding::router_fees::RouterFees::new(
            100_000_000,
            0,
            0,
            rustc_hash::FxHashMap::default(),
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

        // Dummy handles for feed/gas/metrics/router-fees (not running in replay mode). The market
        // event channel stays alive through the `market_event_tx` field on `Solver`.
        let feed_handle = tokio::spawn(futures::future::pending::<()>());
        let gas_price_handle = tokio::spawn(async { /* no-op */ });
        let metrics_sampler_handle = tokio::spawn(async { /* no-op */ });
        let router_fee_handle = tokio::spawn(async { /* no-op */ });

        Ok(Solver {
            router,
            worker_pools,
            market_data,
            derived_data,
            router_fees,
            feed_handle,
            gas_price_handle,
            metrics_sampler_handle,
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
        self.metrics_sampler_handle.abort();
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
            metrics_sampler_handle: self.metrics_sampler_handle,
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
    /// One [`WorkerPool`] per entry configured via [`FyndBuilder::add_pool`].
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
    /// Background task exporting per-protocol market metrics.
    metrics_sampler_handle: JoinHandle<()>,
    /// Background task refreshing router fees from the on-chain FeeCalculator.
    router_fee_handle: JoinHandle<()>,
    /// Background task running the computation manager.
    computation_handle: JoinHandle<()>,
    /// Send a unit value on this channel to trigger a graceful computation-manager shutdown.
    computation_shutdown_tx: broadcast::Sender<()>,
    /// Chain this solver is configured for.
    chain: Chain,
    /// Address of the Tycho Router contract on this chain, or `None` on a quote-only chain.
    router_address: Option<Bytes>,
}

impl SolverParts {
    /// Returns the chain this solver is configured for.
    pub fn chain(&self) -> Chain {
        self.chain
    }

    /// Returns the Tycho Router contract address for this chain, or `None` on a quote-only chain.
    pub fn router_address(&self) -> Option<&Bytes> {
        self.router_address.as_ref()
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
            self.metrics_sampler_handle,
            self.router_fee_handle,
            self.computation_handle,
            self.computation_shutdown_tx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unscoped pool resolves to `PublicOnly` — exclusive components are filtered out unless
    /// a pool explicitly opts in with `IncludeExclusive`.
    #[test]
    fn test_unscoped_pool_resolves_to_public_only() {
        let config = PoolConfig::new("most_liquid");
        assert_eq!(config.liquidity_scope(), None);
        assert_eq!(
            config
                .liquidity_scope()
                .unwrap_or_default(),
            LiquidityScope::PublicOnly
        );
    }

    /// A deployment of nothing but exclusive-access pools would serve requests without access
    /// from no pool at all.
    #[test]
    fn test_build_all_exclusive_pools() {
        let config =
            PoolConfig::new("most_liquid").with_liquidity_scope(LiquidityScope::IncludeExclusive);
        let result = FyndBuilder::new(
            Chain::Ethereum,
            "wss://example.invalid",
            "https://example.invalid",
            vec!["uniswap_v2".to_string()],
            100.0,
        )
        .add_pool("exclusive", &config)
        .expect("add_pool should accept the config")
        .build();

        assert!(matches!(result, Err(SolverBuildError::NoPublicPool)));
    }
}
