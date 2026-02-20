//! API request and response types.

use fynd_core::{Order, OrderSolution};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use utoipa::ToSchema;

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthStatus {
    /// Whether the service is healthy.
    #[schema(example = true)]
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    #[schema(example = 1250)]
    pub last_update_ms: u64,
    /// Number of active solver pools.
    #[schema(example = 2)]
    pub num_solver_pools: usize,
}

/// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SolutionRequest {
    /// Orders to solve.
    pub orders: Vec<Order>,
    /// Optional solving parameters that apply to all orders.
    #[serde(default)]
    pub options: SolutionOptions,
}

/// Options to customize the solving behavior.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct SolutionOptions {
    /// Timeout in milliseconds. If `None`, uses server default.
    #[schema(example = 2000)]
    pub timeout_ms: Option<u64>,
    /// Minimum number of solver responses to wait for before returning.
    /// If `None` or `0`, waits for all solvers to respond (or timeout).
    ///
    /// Use the `/health` endpoint to check `num_solver_pools` before setting this value.
    /// Values exceeding the number of active solver pools are clamped internally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_responses: Option<usize>,
    /// Maximum gas cost allowed for a solution. Solutions exceeding this are filtered out.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "500000")]
    pub max_gas: Option<BigUint>,
}

/// Complete solution for a [`SolutionRequest`].
///
/// Contains a solution for each order in the request, along with aggregate
/// gas estimates and timing information.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Solution {
    /// Solutions for each order, in the same order as the request.
    pub orders: Vec<OrderSolution>,
    /// Total estimated gas for executing all swaps (as decimal string).
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String, example = "150000")]
    pub total_gas_estimate: BigUint,
    /// Time taken to compute this solution, in milliseconds.
    #[schema(example = 12)]
    pub solve_time_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_solution_serializes_amounts_as_strings() {
        let solution = Solution {
            orders: vec![],
            total_gas_estimate: BigUint::from(500_000u64),
            solve_time_ms: 10,
        };

        let json = serde_json::to_string(&solution).unwrap();
        assert!(json.contains(r#""total_gas_estimate":"500000""#));
    }
}
