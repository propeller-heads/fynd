//! PriceGuard: validates solver outputs against external price sources.

use num_bigint::BigUint;
use num_traits::Zero;
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use tycho_simulation::tycho_common::models::Address;

use super::{
    config::PriceGuardConfig,
    provider::{ExternalPrice, PriceProviderError},
    provider_registry::PriceProviderRegistry,
};
use crate::types::{OrderQuote, QuoteStatus};

/// Errors returned by [`PriceGuard::validate`].
#[derive(Error, Debug)]
pub enum PriceGuardError {
    /// Price guard is enabled but no providers are registered.
    #[error("price guard is enabled but no providers are registered")]
    NoProviders,
}

/// Validates solver outputs against external price sources.
///
/// Queries all registered providers concurrently and checks each provider's price individually
/// against the BPS tolerance. A solution passes if **at least one** provider's price is within
/// tolerance. Only rejects if no provider validates.
///
/// Owns the background worker handles for each provider and aborts them on drop.
pub struct PriceGuard {
    registry: PriceProviderRegistry,
    worker_handles: Vec<JoinHandle<()>>,
}

impl Drop for PriceGuard {
    fn drop(&mut self) {
        for handle in &self.worker_handles {
            handle.abort();
        }
    }
}

impl PriceGuard {
    pub fn new(registry: PriceProviderRegistry, worker_handles: Vec<JoinHandle<()>>) -> Self {
        Self { registry, worker_handles }
    }

    /// Validates a list of order quotes against external prices.
    ///
    /// For each successful quote with a route:
    /// 1. Checks that the quote has a well-formed route
    /// 2. Queries all registered providers
    /// 3. Passes if at least one provider validates within BPS tolerance
    ///
    /// Failures are always per-quote: the quote's status is set to
    /// `PriceCheckFailed` and processing continues. Never aborts the batch.
    pub fn validate(
        &self,
        mut quotes: Vec<OrderQuote>,
        config: &PriceGuardConfig,
    ) -> Result<Vec<OrderQuote>, PriceGuardError> {
        if !config.enabled() {
            return Ok(quotes);
        }

        if self.registry.is_empty() {
            return Err(PriceGuardError::NoProviders);
        }

        for quote in &mut quotes {
            if quote.status() != QuoteStatus::Success {
                continue;
            }
            let Some((token_in, token_out)) = self.validated_token_pair(quote) else {
                // This should not happen.
                quote.set_status(QuoteStatus::NoRouteFound);
                continue;
            };
            if !self.check_price(quote, &token_in, &token_out, config) {
                quote.set_status(QuoteStatus::PriceCheckFailed);
            }
        }

        Ok(quotes)
    }

    /// Checks that a successful quote has a route with input/output tokens.
    /// Returns the token pair if valid, `None` otherwise.
    fn validated_token_pair(&self, quote: &OrderQuote) -> Option<(Address, Address)> {
        //invalid route would be rejected earlier; this prevents using expect
        let Some(route) = quote.route() else {
            warn!(order_id = quote.order_id(), "successful quote has no route");
            return None;
        };
        let (Some(token_in), Some(token_out)) = (route.input_token(), route.output_token()) else {
            warn!(order_id = quote.order_id(), "successful quote has empty route");
            return None;
        };
        Some((token_in, token_out))
    }

    /// Queries all providers and returns `true` if at least one validates.
    fn check_price(
        &self,
        quote: &OrderQuote,
        token_in: &Address,
        token_out: &Address,
        config: &PriceGuardConfig,
    ) -> bool {
        let results = self
            .registry
            .get_all_expected_out(token_in, token_out, quote.amount_in());

        let mut price_out_of_tolerance = false;
        let mut has_provider_error = false;

        for result in &results {
            match result {
                Ok(price) => {
                    if self.price_within_tolerance(quote, price, config) {
                        return true;
                    }
                    price_out_of_tolerance = true;
                }
                Err(e) => {
                    if let PriceProviderError::Unavailable(_)
                    | PriceProviderError::TokenNotFound { .. }
                    | PriceProviderError::StaleData { .. } = e
                    {
                        has_provider_error = true;
                    }
                    debug!(error = %e, "price provider error");
                }
            }
        }
        if price_out_of_tolerance {
            return false;
        }
        if has_provider_error {
            config.allow_on_provider_error()
        } else {
            config.allow_on_token_price_not_found()
        }
    }

    /// Returns `true` if the quote's output is within the BPS tolerance of the external price.
    fn price_within_tolerance(
        &self,
        quote: &OrderQuote,
        provider_price: &ExternalPrice,
        config: &PriceGuardConfig,
    ) -> bool {
        if provider_price
            .expected_amount_out()
            .is_zero()
        {
            return false;
        }

        let provider_amount_out = provider_price.expected_amount_out();
        let fynd_amount_out = quote.amount_out();

        let (diff, tolerance) = if fynd_amount_out >= provider_amount_out {
            (fynd_amount_out - provider_amount_out, config.upper_tolerance_bps())
        } else {
            (provider_amount_out - fynd_amount_out, config.lower_tolerance_bps())
        };

        let deviation_bps: u32 = ((&diff * BigUint::from(10_000u32)) / provider_amount_out)
            .try_into()
            .unwrap_or(u32::MAX);

        if deviation_bps <= tolerance {
            return true;
        }

        debug!(
            source = provider_price.source(),
            deviation_bps,
            tolerance,
            expected_out = %provider_amount_out,
            tycho_price = %fynd_amount_out,
            "price check failed for provider"
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use rstest::rstest;
    use tokio::task::JoinHandle;
    use tycho_simulation::{
        evm::tycho_models::Chain,
        tycho_common::models::Address,
        tycho_core::{models::token::Token, Bytes},
    };

    use super::{PriceGuard, PriceGuardError};
    use crate::{
        algorithm::test_utils::{component, MockProtocolSim},
        feed::market_data::SharedMarketDataRef,
        price_guard::{
            config::PriceGuardConfig,
            provider::{ExternalPrice, PriceProvider, PriceProviderError},
            provider_registry::PriceProviderRegistry,
        },
        types::{BlockInfo, OrderQuote, QuoteStatus, Route, Swap},
    };

    struct MockProvider {
        expected_out: BigUint,
        source: String,
    }

    impl PriceProvider for MockProvider {
        fn start(&mut self, _market_data: SharedMarketDataRef) -> JoinHandle<()> {
            tokio::spawn(std::future::ready(()))
        }

        fn get_expected_out(
            &self,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: &BigUint,
        ) -> Result<ExternalPrice, PriceProviderError> {
            Ok(ExternalPrice::new(self.expected_out.clone(), self.source.clone(), 1000))
        }
    }

    struct FailingProvider;

    impl PriceProvider for FailingProvider {
        fn start(&mut self, _market_data: SharedMarketDataRef) -> JoinHandle<()> {
            tokio::spawn(std::future::ready(()))
        }

        fn get_expected_out(
            &self,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: &BigUint,
        ) -> Result<ExternalPrice, PriceProviderError> {
            Err(PriceProviderError::Unavailable("test failure".into()))
        }
    }

    struct PriceNotFoundProvider;

    impl PriceProvider for PriceNotFoundProvider {
        fn start(&mut self, _market_data: SharedMarketDataRef) -> JoinHandle<()> {
            tokio::spawn(std::future::ready(()))
        }

        fn get_expected_out(
            &self,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: &BigUint,
        ) -> Result<ExternalPrice, PriceProviderError> {
            Err(PriceProviderError::PriceNotFound {
                token_in: "0xdead".to_string(),
                token_out: "0xbeef".to_string(),
            })
        }
    }

    fn make_token(address: Address, symbol: &str) -> Token {
        Token {
            address,
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn weth_usdc_swap() -> Swap {
        let weth_addr = Address::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let usdc_addr = Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let weth_token = make_token(weth_addr.clone(), "WETH");
        let usdc_token = make_token(usdc_addr.clone(), "USDC");
        Swap::new(
            "weth-usdc-pool".to_string(),
            "uniswap_v2".to_string(),
            weth_addr,
            usdc_addr,
            BigUint::from(1000u64),
            BigUint::from(950u64),
            BigUint::from(100_000u64),
            component("weth-usdc-pool", &[weth_token, usdc_token]),
            Box::new(MockProtocolSim::default()),
        )
    }

    fn make_quote(amount_out: u64) -> OrderQuote {
        OrderQuote::new(
            "order-1".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(amount_out),
            BigUint::from(100_000u64),
            BigUint::from(amount_out),
            BlockInfo::new(1, "0xabc".to_string(), 1000),
            "test".to_string(),
            Bytes::from([0xAA; 20].as_slice()),
            Bytes::from([0xBB; 20].as_slice()),
        )
        .with_route(Route::new(vec![weth_usdc_swap()]))
    }

    fn price_guard(providers: Vec<Box<dyn PriceProvider>>) -> PriceGuard {
        let mut registry = PriceProviderRegistry::new();
        for p in providers {
            registry = registry.register(p);
        }
        PriceGuard::new(registry, vec![])
    }

    fn mock_provider(expected_out: u64) -> Box<dyn PriceProvider> {
        Box::new(MockProvider {
            expected_out: BigUint::from(expected_out),
            source: "mock".to_string(),
        })
    }

    #[rstest]
    // Lower bound: fynd < provider
    #[case::exact_match(1000, 1000, 0, 10_000, true)]
    #[case::within_lower(1000, 970, 300, 10_000, true)]
    #[case::at_lower_boundary(10_000, 9700, 300, 10_000, true)]
    #[case::beyond_lower(1000, 960, 300, 10_000, false)]
    // Upper bound: fynd > provider
    #[case::within_upper(1000, 1500, 300, 10_000, true)]
    #[case::at_upper_boundary(1000, 2000, 300, 10_000, true)]
    #[case::beyond_upper(1000, 2500, 300, 10_000, false)]
    #[test]
    fn test_deviation_bounds(
        #[case] provider_amount: u64,
        #[case] fynd_amount: u64,
        #[case] lower_bps: u32,
        #[case] upper_bps: u32,
        #[case] should_pass: bool,
    ) {
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_lower_tolerance_bps(lower_bps)
            .with_upper_tolerance_bps(upper_bps);
        let guard = price_guard(vec![mock_provider(provider_amount)]);

        let result = guard
            .validate(vec![make_quote(fynd_amount)], &config)
            .unwrap();

        let expected_status =
            if should_pass { QuoteStatus::Success } else { QuoteStatus::PriceCheckFailed };
        assert_eq!(result[0].status(), expected_status);
    }

    #[rstest]
    #[case::all_error_allow(true, true)]
    #[case::all_error_deny(false, false)]
    #[test]
    fn test_all_providers_error(#[case] allow_on_error: bool, #[case] should_pass: bool) {
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_allow_on_provider_error(allow_on_error);
        let guard = price_guard(vec![Box::new(FailingProvider), Box::new(FailingProvider)]);

        let result = guard
            .validate(vec![make_quote(500)], &config)
            .unwrap();

        let want = if should_pass { QuoteStatus::Success } else { QuoteStatus::PriceCheckFailed };
        assert_eq!(result[0].status(), want);
    }

    #[test]
    fn test_disabled_guard() {
        let config = PriceGuardConfig::default().with_enabled(false);

        // Guard is disabled via config. Expected amount out of the provider is irrelevant,
        // because the provider is never called.
        let guard = price_guard(vec![mock_provider(10_000)]);

        let result = guard
            .validate(vec![make_quote(50)], &config)
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status(), QuoteStatus::Success);
    }

    #[test]
    fn test_one_pass_one_fail() {
        // Test that the quote status is success even with one failing provider,
        // as long as the second provider passes.
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_lower_tolerance_bps(300);

        // Our amount out is below the acceptable lower bound of the first provider,
        // but passes with the second.
        let guard = price_guard(vec![mock_provider(1000), mock_provider(970)]);

        let result = guard
            .validate(vec![make_quote(960)], &config)
            .unwrap();

        assert_eq!(result[0].status(), QuoteStatus::Success);
    }

    #[test]
    fn test_one_provider_failure() {
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_lower_tolerance_bps(300);
        let guard = price_guard(vec![Box::new(FailingProvider), mock_provider(1000)]);

        let result = guard
            .validate(vec![make_quote(980)], &config)
            .unwrap();

        assert_eq!(result[0].status(), QuoteStatus::Success);
    }

    #[test]
    fn test_failed_quote() {
        // Test that the QuoteStatus::NoRouteFound remains unchanged
        let config = PriceGuardConfig::default().with_enabled(true);
        let guard = price_guard(vec![mock_provider(10_000_000)]);

        let mut quote = make_quote(1);
        quote.set_status(QuoteStatus::NoRouteFound);

        let result = guard
            .validate(vec![quote], &config)
            .unwrap();

        assert_eq!(result[0].status(), QuoteStatus::NoRouteFound);
    }

    #[test]
    fn test_no_providers_returns_error() {
        let config = PriceGuardConfig::default().with_enabled(true);
        let guard = price_guard(vec![]);

        let result = guard.validate(vec![make_quote(1000)], &config);

        assert!(matches!(result, Err(PriceGuardError::NoProviders)));
    }

    #[test]
    fn test_multiple_quotes() {
        // Test that multiple quotes get statuses independent of each other.
        // For example - one passes and one fails.
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_lower_tolerance_bps(300);
        let guard = price_guard(vec![mock_provider(1000)]);

        let result = guard
            .validate(vec![make_quote(980), make_quote(500)], &config)
            .unwrap();

        assert_eq!(result[0].status(), QuoteStatus::Success);
        assert_eq!(result[1].status(), QuoteStatus::PriceCheckFailed);
    }

    #[rstest]
    #[case::allow(true, QuoteStatus::Success)]
    #[case::deny(false, QuoteStatus::PriceCheckFailed)]
    #[test]
    fn test_all_price_not_found(#[case] allow: bool, #[case] result_status: QuoteStatus) {
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_allow_on_token_price_not_found(allow);
        let guard =
            price_guard(vec![Box::new(PriceNotFoundProvider), Box::new(PriceNotFoundProvider)]);

        let result = guard
            .validate(vec![make_quote(500)], &config)
            .unwrap();

        assert_eq!(result[0].status(), result_status);
    }

    #[test]
    fn test_mixed_price_not_found_and_error() {
        // When at least one provider has an infrastructure error, the token
        // might be supported but the provider is just down — fall back to
        // allow_on_provider_error.
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_allow_on_token_price_not_found(true)
            .with_allow_on_provider_error(false);
        let guard = price_guard(vec![Box::new(PriceNotFoundProvider), Box::new(FailingProvider)]);

        let result = guard
            .validate(vec![make_quote(500)], &config)
            .unwrap();

        assert_eq!(result[0].status(), QuoteStatus::PriceCheckFailed);
    }

    #[test]
    fn test_price_not_found_ignores_provider_error() {
        // allow_on_provider_error should not allow price-not-found cases.
        let config = PriceGuardConfig::default()
            .with_enabled(true)
            .with_allow_on_provider_error(true)
            .with_allow_on_token_price_not_found(false);
        let guard =
            price_guard(vec![Box::new(PriceNotFoundProvider), Box::new(PriceNotFoundProvider)]);

        let result = guard
            .validate(vec![make_quote(500)], &config)
            .unwrap();

        assert_eq!(result[0].status(), QuoteStatus::PriceCheckFailed);
    }
}
