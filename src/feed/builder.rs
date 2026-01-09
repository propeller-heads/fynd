//! Tycho feed builder.
//!
//! This module provides a builder for the TychoFeed and its configuration.
//! Uses the typestate pattern (via typed-builder) to enforce required fields at compile time.

use std::time::Duration;

use tokio::sync::broadcast::Receiver;
use typed_builder::TypedBuilder;

use crate::{
    api::HealthTracker, feed::TychoFeedConfig, MarketEvent, SharedMarketDataRef, TychoFeed,
};

/// Builder for TychoFeed using typestate pattern to enforce required fields.
///
/// # Required fields (enforced at compile time)
/// - `tycho_url`
/// - `tycho_api_key`
/// - `rpc_url`
/// - `protocols`
/// - `min_tvl` and `max_tvl`
/// - `market_data`
/// - `health_tracker`
///
/// # Optional fields (have defaults)
/// - `gas_refresh_interval` (defaults to 30s)
/// - `reconnect_delay` (defaults to 5s)
///
/// # Example
/// ```ignore
/// let (feed, events) = TychoFeedBuilder::builder()
///     .tycho_api_key("key")
///     .rpc_url("http://localhost:8545")
///     .protocols(vec!["uniswap_v2".into()])
///     .min_tvl(10.0)
///     .max_tvl(1_000_000.0)
///     .market_data(data)
///     .health_tracker(tracker)
///     .build();
/// ```
#[derive(TypedBuilder)]
#[builder(build_method(into))]
pub struct TychoFeedBuilder {
    /// Tycho WebSocket URL (required).
    #[builder(setter(into))]
    tycho_url: String,

    /// Tycho API key (required).
    #[builder(setter(into))]
    tycho_api_key: String,

    /// RPC URL for gas price fetching (required).
    #[builder(setter(into))]
    rpc_url: String,

    /// Protocols to index (required).
    protocols: Vec<String>,

    /// Minimum TVL filter in ETH (required).
    min_tvl: f64,

    /// Maximum TVL filter in ETH (required).
    max_tvl: f64,

    /// Gas price refresh interval (optional, defaults to 30s).
    #[builder(default = Duration::from_secs(30))]
    gas_refresh_interval: Duration,

    /// Reconnect delay on connection failure (optional, defaults to 5s).
    #[builder(default = Duration::from_secs(5))]
    reconnect_delay: Duration,

    /// Market data shared reference (required).
    market_data: SharedMarketDataRef,

    /// Health tracker (required).
    health_tracker: HealthTracker,
}

impl From<TychoFeedBuilder> for (TychoFeed, Receiver<MarketEvent>) {
    fn from(builder: TychoFeedBuilder) -> Self {
        let config = TychoFeedConfig {
            tycho_url: builder.tycho_url,
            tycho_api_key: builder.tycho_api_key,
            protocols: builder.protocols,
            min_tvl: builder.min_tvl,
            max_tvl: builder.max_tvl,
            rpc_url: builder.rpc_url,
            gas_refresh_interval: builder.gas_refresh_interval,
            reconnect_delay: builder.reconnect_delay,
        };

        TychoFeed::new(config, builder.market_data, builder.health_tracker)
    }
}
