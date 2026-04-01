//! Registry for managing multiple [PriceProvider](crate::price_guard::provider::PriceProvider)s.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Address;

use super::{
    binance_ws::BinanceWsProvider,
    hyperliquid::HyperliquidProvider,
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
};

/// Manages multiple [`PriceProvider`]s and queries them.
pub struct PriceProviderRegistry {
    providers: Vec<Box<dyn PriceProvider>>,
}

impl PriceProviderRegistry {
    /// Create an empty registry with no providers registered.
    pub fn new() -> Self {
        Self { providers: Vec::new() }
    }

    /// Registers a price provider.
    pub fn register(mut self, provider: Box<dyn PriceProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// Registers the built-in providers (Hyperliquid + Binance).
    pub fn with_default_providers(self) -> Self {
        self.register(Box::new(HyperliquidProvider::default()))
            .register(Box::new(BinanceWsProvider::default()))
    }

    /// Returns `true` if no providers are registered.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Queries all providers for the expected output amount.
    pub fn get_all_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Vec<Result<ExternalPrice, PriceProviderError>> {
        self.providers
            .iter()
            .map(|p| p.get_expected_out(token_in, token_out, amount_in))
            .collect()
    }
}

impl Default for PriceProviderRegistry {
    fn default() -> Self {
        Self::new().with_default_providers()
    }
}
