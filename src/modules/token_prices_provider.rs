use num_bigint::BigUint;
use std::collections::HashMap;
use tycho_simulation::tycho_core::Bytes;

/// Token price provider error types
#[derive(Debug)]
pub enum TokenPricesProviderError {
    Config(String),
    Network(String),
    Parsing(String),
    PriceNotAvailable(String),
    External(String),
}

impl std::fmt::Display for TokenPricesProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::Network(msg) => write!(f, "Network request failed: {}", msg),
            Self::Parsing(msg) => write!(f, "Data parsing failed: {}", msg),
            Self::PriceNotAvailable(msg) => write!(f, "Price not available: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for TokenPricesProviderError {}

/// Fetches token prices from external APIs
pub struct TokenPricesProvider {
    // Could add configuration like API keys, endpoints, cache settings, etc.
}

impl TokenPricesProvider {
    pub fn new() -> Self {
        Self {}
    }

    /// Fetch token prices from external price feeds
    pub async fn fetch_token_prices(
        &self,
        _token_addresses: &[Bytes],
    ) -> Result<HashMap<Bytes, BigUint>, TokenPricesProviderError> {
        // TODO: Implement actual price fetching
        // This could call:
        // - CoinGecko API
        // - CoinMarketCap API
        // - DeFi Pulse API
        // - 1inch API
        // - 0x API
        // - On-chain price oracles (Chainlink, etc.)

        todo!()
    }

    /// Fetch price for a single token
    pub async fn fetch_token_price(
        &self,
        token_address: &Bytes,
    ) -> Result<Option<BigUint>, TokenPricesProviderError> {
        let prices = self.fetch_token_prices(&[token_address.clone()]).await?;
        Ok(prices.get(token_address).cloned())
    }
}
