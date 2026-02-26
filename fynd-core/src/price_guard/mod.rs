pub mod binance_ws;
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
