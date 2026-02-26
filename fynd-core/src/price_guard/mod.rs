pub mod binance_ws;
pub mod chainlink;
pub mod common;
pub mod config;
pub mod hyperliquid;
pub mod provider;

use num_bigint::BigUint;
use num_traits::Zero;
use tracing::{debug, warn};

use self::{config::PriceGuardConfig, provider::PriceProviderRegistry};
use crate::types::{OrderSolution, SolutionStatus, SolveError};

/// Validates solution outputs against external price sources.
///
/// Queries all registered providers concurrently and checks each provider's price
/// individually against the BPS tolerance. A solution passes if **at least one**
/// provider's price is within tolerance. Only rejects if no provider validates.
pub struct PriceGuard {
    registry: PriceProviderRegistry,
    config: PriceGuardConfig,
}

impl PriceGuard {
    pub fn new(registry: PriceProviderRegistry, config: PriceGuardConfig) -> Self {
        Self { registry, config }
    }

    /// Validates a list of order solutions against external prices.
    ///
    /// For each successful solution with a route:
    /// 1. Queries all registered providers concurrently
    /// 2. Checks each provider's price against the BPS tolerance individually
    /// 3. Passes if at least one provider validates; rejects only if none do
    ///
    /// Solutions that are not `Success`, have no route, or where the user gets
    /// more than expected are passed through unchanged.
    ///
    /// # Error semantics
    ///
    /// Two distinct failure modes:
    /// - **Per-solution**: When providers return prices but none validate within
    ///   tolerance, the solution's status is set to `PriceCheckFailed` and
    ///   processing continues with the remaining solutions.
    /// - **Catastrophic**: When *all* providers error and `allow_on_provider_error`
    ///   is `false`, returns `Err(SolveError::PriceCheckFailed)` immediately,
    ///   aborting the entire batch.
    pub async fn validate(
        &self,
        solutions: Vec<OrderSolution>,
    ) -> Result<Vec<OrderSolution>, SolveError> {
        if !self.config.enabled() {
            return Ok(solutions);
        }

        let mut validated = Vec::with_capacity(solutions.len());

        for mut solution in solutions {
            if solution.status != SolutionStatus::Success {
                validated.push(solution);
                continue;
            }

            let (token_in, token_out) = match &solution.route {
                Some(route) if !route.swaps.is_empty() => (
                    route
                        .swaps
                        .first()
                        .unwrap()
                        .token_in
                        .clone(),
                    route
                        .swaps
                        .last()
                        .unwrap()
                        .token_out
                        .clone(),
                ),
                _ => {
                    validated.push(solution);
                    continue;
                }
            };

            let results = self
                .registry
                .get_all_expected_out(&token_in, &token_out, &solution.amount_in)
                .await;

            let mut any_validated = false;
            let mut all_errors = true;

            for result in &results {
                match result {
                    Ok(external_price) => {
                        all_errors = false;

                        if external_price.expected_amount_out().is_zero() {
                            // Zero price means we can't validate — treat as pass
                            any_validated = true;
                            break;
                        }

                        if solution.amount_out >= *external_price.expected_amount_out() {
                            // User gets more than or equal to expected — always passes
                            any_validated = true;
                            debug!(
                                source = external_price.source(),
                                "price check passed (amount_out >= expected)"
                            );
                            break;
                        }

                        let diff = external_price.expected_amount_out() - &solution.amount_out;
                        let deviation_bps = (&diff * BigUint::from(10_000u32))
                            / external_price.expected_amount_out();
                        let deviation_bps_u32: u32 =
                            deviation_bps.try_into().unwrap_or(u32::MAX);

                        if deviation_bps_u32 <= self.config.tolerance_bps() {
                            any_validated = true;
                            debug!(
                                source = external_price.source(),
                                deviation_bps = deviation_bps_u32,
                                "price check passed"
                            );
                            break;
                        } else {
                            warn!(
                                source = external_price.source(),
                                deviation_bps = deviation_bps_u32,
                                tolerance_bps = self.config.tolerance_bps(),
                                "price check failed for provider"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "price provider error");
                    }
                }
            }

            if all_errors {
                if !self.config.allow_on_provider_error() {
                    return Err(SolveError::PriceCheckFailed {
                        order_id: solution.order_id.clone(),
                        reason: "all price providers failed".to_string(),
                    });
                }
                // All providers errored but allow_on_provider_error is true — pass through
            } else if !any_validated {
                solution.status = SolutionStatus::PriceCheckFailed;
            }

            validated.push(solution);
        }

        Ok(validated)
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use num_bigint::BigUint;
    use tycho_simulation::tycho_common::models::Address;

    use super::{
        config::PriceGuardConfig,
        provider::{ExternalPrice, PriceProvider, PriceProviderError, PriceProviderRegistry},
        PriceGuard,
    };
    use crate::types::{
        solution::{BlockInfo, Route, Swap},
        OrderSolution, SolutionStatus, SolveError,
    };

    // -- Mock providers -------------------------------------------------------

    /// Returns a fixed expected_amount_out for any query.
    struct FixedProvider {
        expected_out: BigUint,
        source: String,
    }

    #[async_trait]
    impl PriceProvider for FixedProvider {
        async fn get_expected_out(
            &self,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: &BigUint,
        ) -> Result<ExternalPrice, PriceProviderError> {
            Ok(ExternalPrice::new(
                self.expected_out.clone(),
                self.source.clone(),
                1000,
            ))
        }
    }

    /// Always returns an error.
    struct FailingProvider;

    #[async_trait]
    impl PriceProvider for FailingProvider {
        async fn get_expected_out(
            &self,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: &BigUint,
        ) -> Result<ExternalPrice, PriceProviderError> {
            Err(PriceProviderError::Unavailable("test failure".into()))
        }
    }

    /// Returns expected_amount_out of zero.
    struct ZeroPriceProvider;

    #[async_trait]
    impl PriceProvider for ZeroPriceProvider {
        async fn get_expected_out(
            &self,
            _token_in: &Address,
            _token_out: &Address,
            _amount_in: &BigUint,
        ) -> Result<ExternalPrice, PriceProviderError> {
            Ok(ExternalPrice::new(
                BigUint::ZERO,
                "zero".to_string(),
                1000,
            ))
        }
    }

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn make_swap(token_in: u8, token_out: u8) -> Swap {
        Swap {
            component_id: "pool-1".to_string(),
            protocol: "uniswap_v2".to_string(),
            token_in: addr(token_in),
            token_out: addr(token_out),
            amount_in: BigUint::from(1000u64),
            amount_out: BigUint::from(950u64),
            gas_estimate: BigUint::from(100_000u64),
        }
    }

    fn make_solution(amount_in: u64, amount_out: u64) -> OrderSolution {
        OrderSolution {
            order_id: "order-1".to_string(),
            status: SolutionStatus::Success,
            route: Some(Route::new(vec![make_swap(0x01, 0x02)])),
            amount_in: BigUint::from(amount_in),
            amount_out: BigUint::from(amount_out),
            gas_estimate: BigUint::from(100_000u64),
            price_impact_bps: None,
            amount_out_net_gas: BigUint::from(amount_out),
            block: BlockInfo { number: 1, hash: "0xabc".to_string(), timestamp: 1000 },
            algorithm: "test".to_string(),
            transaction: None,
        }
    }

    fn make_guard(providers: Vec<Box<dyn PriceProvider>>, config: PriceGuardConfig) -> PriceGuard {
        let mut registry = PriceProviderRegistry::new();
        for p in providers {
            registry = registry.register(p);
        }
        PriceGuard::new(registry, config)
    }

    fn fixed(expected_out: u64) -> Box<dyn PriceProvider> {
        Box::new(FixedProvider {
            expected_out: BigUint::from(expected_out),
            source: "fixed".to_string(),
        })
    }

    fn fixed_named(expected_out: u64, name: &str) -> Box<dyn PriceProvider> {
        Box::new(FixedProvider {
            expected_out: BigUint::from(expected_out),
            source: name.to_string(),
        })
    }

    fn failing() -> Box<dyn PriceProvider> {
        Box::new(FailingProvider)
    }

    fn zero_price() -> Box<dyn PriceProvider> {
        Box::new(ZeroPriceProvider)
    }

    #[tokio::test]
    async fn disabled_guard_passes_everything_through() {
        let config = PriceGuardConfig::default().with_enabled(false);
        // Provider that would reject — but guard is disabled, so it's never called.
        let guard = make_guard(vec![fixed(10_000)], config);

        let solutions = vec![make_solution(1000, 50)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn provider_agrees_within_tolerance_passes() {
        // Solution: amount_out = 970. Provider expects 1000. Deviation = 30/1000 = 300 bps.
        // Tolerance = 300 bps → passes exactly.
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(vec![fixed(1000)], config);

        let solutions = vec![make_solution(1000, 970)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn provider_rejects_beyond_tolerance() {
        // Solution: amount_out = 960. Provider expects 1000. Deviation = 40/1000 = 400 bps.
        // Tolerance = 300 bps → fails.
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(vec![fixed(1000)], config);

        let solutions = vec![make_solution(1000, 960)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::PriceCheckFailed);
    }

    #[tokio::test]
    async fn amount_out_exceeds_expected_always_passes() {
        // Solution gets more than the provider expects — no-loss scenario.
        let config = PriceGuardConfig::default().with_tolerance_bps(0);
        let guard = make_guard(vec![fixed(900)], config);

        let solutions = vec![make_solution(1000, 950)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn amount_out_equals_expected_passes() {
        let config = PriceGuardConfig::default().with_tolerance_bps(0);
        let guard = make_guard(vec![fixed(1000)], config);

        let solutions = vec![make_solution(1000, 1000)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn one_provider_validates_despite_other_rejecting() {
        // Provider A expects 1000, solution gives 960 → 400 bps deviation → fails.
        // Provider B expects 970, solution gives 960 → ~103 bps → passes.
        // "At least one validates" → passes.
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(
            vec![fixed_named(1000, "strict"), fixed_named(970, "lenient")],
            config,
        );

        let solutions = vec![make_solution(1000, 960)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn one_provider_fails_other_validates() {
        // First provider errors, second validates → passes.
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(vec![failing(), fixed(1000)], config);

        let solutions = vec![make_solution(1000, 980)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn all_providers_error_allow_on_error_passes_through() {
        let config = PriceGuardConfig::default().with_allow_on_provider_error(true);
        let guard = make_guard(vec![failing(), failing()], config);

        let solutions = vec![make_solution(1000, 500)];
        let result = guard.validate(solutions).await.unwrap();

        // Passed through despite no validation — fail-open.
        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn all_providers_error_deny_on_error_returns_err() {
        let config = PriceGuardConfig::default().with_allow_on_provider_error(false);
        let guard = make_guard(vec![failing(), failing()], config);

        let solutions = vec![make_solution(1000, 500)];
        let err = guard.validate(solutions).await.unwrap_err();

        assert!(matches!(err, SolveError::PriceCheckFailed { .. }));
    }

    #[tokio::test]
    async fn non_success_solutions_pass_through_unchanged() {
        let config = PriceGuardConfig::default();
        // Provider that would fail any price check — but non-success solutions skip it.
        let guard = make_guard(vec![fixed(10_000_000)], config);

        let mut solution = make_solution(1000, 1);
        solution.status = SolutionStatus::NoRouteFound;

        let result = guard.validate(vec![solution]).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::NoRouteFound);
    }

    #[tokio::test]
    async fn solution_without_route_passes_through() {
        let config = PriceGuardConfig::default();
        let guard = make_guard(vec![fixed(10_000_000)], config);

        let mut solution = make_solution(1000, 1);
        solution.route = None;

        let result = guard.validate(vec![solution]).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn solution_with_empty_swaps_passes_through() {
        let config = PriceGuardConfig::default();
        let guard = make_guard(vec![fixed(10_000_000)], config);

        let mut solution = make_solution(1000, 1);
        solution.route = Some(Route::new(vec![]));

        let result = guard.validate(vec![solution]).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn zero_expected_amount_treated_as_pass() {
        // Provider returns zero expected_amount_out → can't validate, treat as pass.
        let config = PriceGuardConfig::default().with_tolerance_bps(0);
        let guard = make_guard(vec![zero_price()], config);

        let solutions = vec![make_solution(1000, 1)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn multiple_solutions_validated_independently() {
        // Provider expects 1000. Tolerance 300 bps.
        // Solution A: amount_out = 980 → 20/1000 = 200 bps → passes.
        // Solution B: amount_out = 500 → 500/1000 = 5000 bps → fails.
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(vec![fixed(1000)], config);

        let solution_a = {
            let mut s = make_solution(1000, 980);
            s.order_id = "order-a".to_string();
            s
        };
        let solution_b = {
            let mut s = make_solution(1000, 500);
            s.order_id = "order-b".to_string();
            s
        };

        let result = guard.validate(vec![solution_a, solution_b]).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
        assert_eq!(result[1].status, SolutionStatus::PriceCheckFailed);
    }

    #[tokio::test]
    async fn no_providers_registered_with_allow_on_error() {
        // Empty registry → get_all_expected_out returns empty vec → all_errors stays true.
        let config = PriceGuardConfig::default().with_allow_on_provider_error(true);
        let guard = make_guard(vec![], config);

        let solutions = vec![make_solution(1000, 500)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn no_providers_registered_deny_on_error() {
        let config = PriceGuardConfig::default().with_allow_on_provider_error(false);
        let guard = make_guard(vec![], config);

        let solutions = vec![make_solution(1000, 500)];
        let err = guard.validate(solutions).await.unwrap_err();

        assert!(matches!(err, SolveError::PriceCheckFailed { .. }));
    }

    #[tokio::test]
    async fn boundary_deviation_exactly_at_tolerance_passes() {
        // amount_out = 9700, expected = 10000 → deviation = 300/10000 = 300 bps.
        // Tolerance = 300 bps → passes (<=, not <).
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(vec![fixed(10_000)], config);

        let solutions = vec![make_solution(10_000, 9700)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::Success);
    }

    #[tokio::test]
    async fn boundary_deviation_one_above_tolerance_fails() {
        // amount_out = 9699, expected = 10000 → deviation = 301/10000 = 301 bps.
        // Tolerance = 300 bps → fails.
        let config = PriceGuardConfig::default().with_tolerance_bps(300);
        let guard = make_guard(vec![fixed(10_000)], config);

        let solutions = vec![make_solution(10_000, 9699)];
        let result = guard.validate(solutions).await.unwrap();

        assert_eq!(result[0].status, SolutionStatus::PriceCheckFailed);
    }
}
