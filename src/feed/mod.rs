use std::time::Duration;

use tycho_simulation::tycho_common::models::Chain;

pub mod events;
pub mod gas;
pub mod market_data;

pub use gas::GAS_PRICE_DEPENDENCY_ID;
pub mod protocol_registry;
pub mod tycho_feed;

/// Configuration for the TychoFeed.
#[derive(Debug, Clone)]
pub(crate) struct TychoFeedConfig {
    /// Tycho WebSocket URL.
    pub(crate) tycho_url: String,
    /// Blockchain to connect to.
    pub(crate) chain: Chain,
    /// Tycho API key (optional).
    pub(crate) tycho_api_key: Option<String>,
    /// Use TLS for Tycho WebSocket connection.
    pub(crate) use_tls: bool,
    /// Names of the protocols to index.
    /// For example, "uniswap_v2", "uniswap_v3", "sushiswap", etc.
    pub(crate) protocols: Vec<String>,
    /// TVL filter in native token, usually ETH.
    /// Components with TVL below this threshold will be ignored/removed from the market data.
    pub(crate) min_tvl: f64,
    /// Minimum token quality filter.
    pub(crate) min_token_quality: i32,
    /// Multiplier used to define the upper bound of the TVL filter.
    /// The upper bound is calculated as `min_tvl * tvl_buffer_multiplier`.
    /// Only components with TVL above this upper bound will be added to the market data.
    /// This approach helps to reduce fluctuations caused by components hovering around a single
    /// threshold.
    /// Default is 1.1 (10% buffer).
    pub(crate) tvl_buffer_multiplier: f64,
    /// RPC URL for the target chain.
    /// Used to fetch gas prices.
    #[allow(dead_code)] //TODO: remove this once we use it (for gas fetching)
    pub(crate) rpc_url: String,
    /// Gas price refresh interval.
    /// Default is 30 seconds.
    pub(crate) gas_refresh_interval: Duration,
    /// Reconnect delay on connection failure.
    /// Default is 5 seconds.
    pub(crate) reconnect_delay: Duration,
}

impl TychoFeedConfig {
    pub fn new(
        tycho_url: String,
        chain: Chain,
        tycho_api_key: Option<String>,
        use_tls: bool,
        protocols: Vec<String>,
        min_tvl: f64,
        rpc_url: String,
    ) -> Self {
        Self {
            tycho_url,
            chain,
            tycho_api_key,
            use_tls,
            protocols,
            min_tvl,
            min_token_quality: 100,
            tvl_buffer_multiplier: 1.1,
            rpc_url,
            gas_refresh_interval: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(5),
        }
    }

    pub fn tvl_buffer_multiplier(mut self, tvl_buffer_multiplier: f64) -> Self {
        self.tvl_buffer_multiplier = tvl_buffer_multiplier;
        self
    }

    pub fn gas_refresh_interval(mut self, gas_refresh_interval: Duration) -> Self {
        self.gas_refresh_interval = gas_refresh_interval;
        self
    }

    pub fn reconnect_delay(mut self, reconnect_delay: Duration) -> Self {
        self.reconnect_delay = reconnect_delay;
        self
    }

    pub fn min_token_quality(mut self, min_token_quality: i32) -> Self {
        self.min_token_quality = min_token_quality;
        self
    }
}

/// Errors that can occur in the indexer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DataFeedError {
    #[error("gas price fetcher error: {0}")]
    GasPriceFetcherError(String),

    /// Market data lock error.
    #[error("failed to acquire market data lock")]
    #[allow(dead_code)]
    LockError,

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),

    /// Update error.
    #[error("stream error: {0}")]
    StreamError(String),

    /// Event send error.
    #[error("event send error: {0}")]
    EventChannelError(String),
}
