//! Price provider trait and implementations.
//!
//! Defines the [`PriceProvider`] trait for fetching external token prices,
//! along with error types.

use std::sync::Arc;

use async_trait::async_trait;
use num_bigint::BigUint;
use tokio::{sync::RwLock, task::JoinHandle};
use tycho_simulation::tycho_common::models::Address;

use crate::feed::market_data::SharedMarketData;

/// Errors that can occur when fetching external prices.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PriceProviderError {
    /// External price source is unavailable.
    #[error("price source unavailable: {0}")]
    Unavailable(String),

    /// No price data found for the requested token pair.
    #[error("price not found for pair {token_in} -> {token_out}")]
    PriceNotFound { token_in: String, token_out: String },

    /// Price data is stale (e.g., WebSocket feed hasn't updated recently).
    #[error("price data stale: last update {age_ms}ms ago")]
    StaleData { age_ms: u64 },
}

/// A price quote from an external source.
#[derive(Debug, Clone)]
pub struct ExternalPrice {
    /// Expected output amount for the given input, in output token units.
    expected_amount_out: BigUint,
    /// Name of the price source (e.g., "coingecko", "binance_ws").
    source: String,
    /// Timestamp of the price data in Unix milliseconds.
    timestamp_ms: u64,
}

impl ExternalPrice {
    /// Creates a new external price quote.
    pub fn new(expected_amount_out: BigUint, source: String, timestamp_ms: u64) -> Self {
        Self { expected_amount_out, source, timestamp_ms }
    }

    pub fn expected_amount_out(&self) -> &BigUint {
        &self.expected_amount_out
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }
}

/// Trait for fetching external token prices.
///
/// Implementations can be REST API polling, WebSocket streaming, or any other source.
/// Stateful providers (e.g., a WebSocket feed caching prices in `Arc<RwLock<HashMap>>`)
/// should read from their internal cache in [`get_expected_out`].
///
/// # Lifecycle
///
/// Providers are constructed via their own `new()` method, then [`start`](Self::start) is
/// called once during server startup with the shared market data. Every provider must
/// spawn a background worker and return its handle so the caller can abort it on shutdown.
#[async_trait]
pub trait PriceProvider: Send + Sync + 'static {
    /// Initialize the provider with shared market data and start the background worker.
    ///
    /// Called once during server startup. Returns the background task handle so
    /// the caller can abort it on shutdown.
    fn start(
        &mut self,
        market_data: Arc<RwLock<SharedMarketData>>,
    ) -> JoinHandle<()>;

    /// Returns the expected output amount for a given input.
    ///
    /// # Arguments
    /// * `token_in` - Address of the input token
    /// * `token_out` - Address of the output token
    /// * `amount_in` - Amount of input token (in token units)
    async fn get_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Result<ExternalPrice, PriceProviderError>;
}

/// Registry of price providers that queries all registered sources concurrently.
///
/// Used by [`PriceGuard`] to implement "at least one provider must validate" semantics:
/// each provider's price is independently checked against the BPS tolerance. If *any*
/// provider's price is close enough to the solution's output, the solution passes.
///
/// # Example
///
/// ```ignore
/// let registry = PriceProviderRegistry::new()
///     .register(Box::new(hyperliquid_provider))
///     .register(Box::new(binance_provider));
/// ```
pub struct PriceProviderRegistry {
    providers: Vec<Box<dyn PriceProvider>>,
}

impl PriceProviderRegistry {
    pub fn new() -> Self {
        Self { providers: Vec::new() }
    }

    /// Registers a price provider. Providers are queried concurrently during validation.
    pub fn register(mut self, provider: Box<dyn PriceProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// Queries all registered providers concurrently and returns individual results.
    ///
    /// Each entry in the returned `Vec` corresponds to one provider's result.
    /// The caller decides how to interpret the results (e.g., pass if at least one validates).
    pub async fn get_all_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Vec<Result<ExternalPrice, PriceProviderError>> {
        let futures: Vec<_> = self
            .providers
            .iter()
            .map(|p| p.get_expected_out(token_in, token_out, amount_in))
            .collect();
        futures::future::join_all(futures).await
    }
}
