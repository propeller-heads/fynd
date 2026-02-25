pub mod config;
pub mod provider;

use metrics::counter;
use num_bigint::BigUint;
use num_traits::Zero;
use tracing::{debug, warn};

use self::{config::PriceGuardConfig, provider::PriceProvider};
use crate::types::{OrderSolution, SolutionStatus, SolveError};

/// Validates solution outputs against external price sources.
pub struct PriceGuard {
    provider: Box<dyn PriceProvider>,
    config: PriceGuardConfig,
}

impl PriceGuard {
    pub fn new(provider: Box<dyn PriceProvider>, config: PriceGuardConfig) -> Self {
        Self { provider, config }
    }

    /// Validates a list of order solutions against external prices.
    ///
    /// For each successful solution with a route:
    /// 1. Fetches the expected output from the external price provider
    /// 2. Computes deviation if the solution gives less than expected
    /// 3. Rejects solutions that deviate beyond the configured tolerance
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

            match self
                .provider
                .get_expected_out(&token_in, &token_out, &solution.amount_in)
                .await
            {
                Ok(external_price) => {
                    if external_price
                        .expected_amount_out()
                        .is_zero()
                    {
                        validated.push(solution);
                        continue;
                    }

                    // Flag when user is getting less than expected
                    if solution.amount_out < *external_price.expected_amount_out() {
                        let diff = external_price.expected_amount_out() - &solution.amount_out;
                        let deviation_bps = (&diff * BigUint::from(10_000u32)) /
                            external_price.expected_amount_out();

                        let deviation_bps_u32: u32 = deviation_bps
                            .try_into()
                            .unwrap_or(u32::MAX);

                        if deviation_bps_u32 > self.config.tolerance_bps() {
                            solution.status = SolutionStatus::PriceCheckFailed;
                        }
                    }
                }
                Err(e) => {
                    if !self.config.allow_on_provider_error() {
                        return Err(SolveError::PriceCheckFailed {
                            order_id: solution.order_id.clone(),
                            reason: format!("price provider error: {}", e),
                        });
                    }
                }
            }

            validated.push(solution);
        }

        Ok(validated)
    }
}
