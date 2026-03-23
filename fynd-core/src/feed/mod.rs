use std::{collections::HashSet, time::Duration};

use tycho_simulation::tycho_common::models::Chain;

pub(crate) mod events;
pub mod gas;
pub mod market_data;
#[cfg(test)]
mod write_lock_contention_test;

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
    /// Ratio used to define the lower bound of the TVL filter for hysteresis.
    /// The lower bound is calculated as `min_tvl / tvl_buffer_ratio`.
    /// Components are added when TVL >= `min_tvl` and removed when TVL drops below
    /// `min_tvl / tvl_buffer_ratio`.
    /// Default is 1.1 (10% buffer).
    pub(crate) tvl_buffer_ratio: f64,
    /// Gas price refresh interval.
    /// Default is 30 seconds.
    pub(crate) gas_refresh_interval: Duration,
    /// Reconnect delay on connection failure.
    /// Default is 5 seconds.
    pub(crate) reconnect_delay: Duration,
    /// Only include tokens traded within this many days.
    pub(crate) traded_n_days_ago: Option<u64>,
    /// Component IDs to exclude from routing.
    pub(crate) blacklisted_components: HashSet<String>,
}

impl TychoFeedConfig {
    pub(crate) fn new(
        tycho_url: String,
        chain: Chain,
        tycho_api_key: Option<String>,
        use_tls: bool,
        protocols: Vec<String>,
        min_tvl: f64,
    ) -> Self {
        Self {
            tycho_url,
            chain,
            tycho_api_key,
            use_tls,
            protocols,
            min_tvl,
            min_token_quality: 100,
            traded_n_days_ago: None,
            tvl_buffer_ratio: 1.1,
            gas_refresh_interval: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(5),
            blacklisted_components: HashSet::new(),
        }
    }

    pub(crate) fn tvl_buffer_ratio(mut self, tvl_buffer_ratio: f64) -> Self {
        self.tvl_buffer_ratio = tvl_buffer_ratio;
        self
    }

    pub(crate) fn gas_refresh_interval(mut self, gas_refresh_interval: Duration) -> Self {
        self.gas_refresh_interval = gas_refresh_interval;
        self
    }

    pub(crate) fn reconnect_delay(mut self, reconnect_delay: Duration) -> Self {
        self.reconnect_delay = reconnect_delay;
        self
    }

    pub(crate) fn min_token_quality(mut self, min_token_quality: i32) -> Self {
        self.min_token_quality = min_token_quality;
        self
    }

    pub(crate) fn traded_n_days_ago(mut self, days: u64) -> Self {
        self.traded_n_days_ago = Some(days);
        self
    }

    pub(crate) fn blacklisted_components(mut self, components: HashSet<String>) -> Self {
        self.blacklisted_components = components;
        self
    }
}

/// Errors that can occur in the indexer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DataFeedError {
    #[error("gas price fetcher error: {0}")]
    GasPriceFetcherError(String),

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
