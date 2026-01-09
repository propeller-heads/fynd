use std::time::Duration;

pub mod builder;
pub mod events;
pub mod market_data;
pub mod tycho_feed;

/// Configuration for the TychoFeed.
#[derive(Debug, Clone)]
pub struct TychoFeedConfig {
    /// Tycho WebSocket URL.
    pub tycho_url: String,
    /// Tycho API key.
    pub tycho_api_key: String,
    /// Protocols to index.
    pub protocols: Vec<String>,
    /// Minimum TVL filter (in ETH).
    pub min_tvl: f64,
    /// Maximum TVL filter (in ETH).
    pub max_tvl: f64,
    /// RPC URL for gas price fetching.
    pub rpc_url: String,
    /// Gas price refresh interval.
    pub gas_refresh_interval: Duration,
    /// Reconnect delay on connection failure.
    pub reconnect_delay: Duration,
}

/// Errors that can occur in the indexer.
#[derive(Debug, thiserror::Error)]
pub enum TychoFeedError {
    /// Connection error.
    #[error("connection error: {0}")]
    Connection(String),

    /// Protocol error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Market data lock error.
    #[error("failed to acquire market data lock")]
    LockError,

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),
}
