//! Price provider trait and types.
//!
//! Defines the [`provider::PriceProvider`](crate::price_guard::provider::PriceProvider) trait for
//! fetching external token prices and supporting error and result types.

use async_trait::async_trait;
use num_bigint::BigUint;
use thiserror::Error;
use tokio::task::JoinHandle;
use tycho_simulation::tycho_common::models::Address;

use crate::feed::market_data::SharedMarketDataRef;

/// Errors that can occur when fetching external prices.
#[derive(Error, Debug, Clone)]
pub enum PriceProviderError {
    /// External price source is unavailable.
    #[error("price source unavailable: {0}")]
    Unavailable(String),

    /// Token address not found in the market data registry.
    #[error("token not found: {address}")]
    TokenNotFound {
        /// Hex-encoded address of the missing token.
        address: String,
    },

    /// No price data found for the requested token pair.
    #[error("price not found for pair {token_in} -> {token_out}")]
    PriceNotFound {
        /// Input token identifier.
        token_in: String,
        /// Output token identifier.
        token_out: String,
    },

    /// Price data is stale (e.g., feed hasn't updated recently).
    #[error("price data stale: last update {age_ms}ms ago")]
    StaleData {
        /// Milliseconds since the last successful price update.
        age_ms: u64,
    },
}

/// A price quote from an external source.
#[derive(Debug, Clone)]
pub struct ExternalPrice {
    /// Expected output amount for the given input, in raw output token units.
    expected_amount_out: BigUint,
    /// Name of the price source (e.g., "hyperliquid", "binance_ws").
    source: String,
    /// Timestamp of the price data in Unix milliseconds.
    timestamp_ms: u64,
}

impl ExternalPrice {
    /// Create a new external price quote.
    pub fn new(expected_amount_out: BigUint, source: String, timestamp_ms: u64) -> Self {
        Self { expected_amount_out, source, timestamp_ms }
    }

    /// Expected output amount in raw output token units.
    pub fn expected_amount_out(&self) -> &BigUint {
        &self.expected_amount_out
    }

    /// Name of the price source (e.g. `"hyperliquid"`, `"binance_ws"`).
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Timestamp of the price data in Unix milliseconds.
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }
}

/// Trait for fetching external token prices.
///
/// Implementations follow the worker+cache pattern: [`start`](PriceProvider::start)
/// spawns a background task that continuously populates an in-memory cache, and
/// [`get_expected_out`](PriceProvider::get_expected_out) reads from that cache
/// without blocking or making network calls.
#[async_trait]
pub trait PriceProvider: Send + Sync + 'static {
    /// Called once at startup. Spawns a background worker that populates an internal cache and
    /// returns its task handle.
    fn start(&mut self, market_data: SharedMarketDataRef) -> JoinHandle<()>;

    /// Returns the expected output amount for a given input by reading from the internal cache.
    /// Must not block or make network calls.
    async fn get_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Result<ExternalPrice, PriceProviderError>;
}
