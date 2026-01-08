//! Solution types returned by the solver.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::models::Address;

use super::primitives::{ComponentId, ProtocolSystem};

/// Complete solution for a solve request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solution {
    /// Solutions for each order in the request.
    pub orders: Vec<OrderSolution>,
    /// Total estimated gas for all swaps.
    pub total_gas_estimate: BigUint,
    /// Time taken to solve in milliseconds.
    pub solve_time_ms: u64,
}

/// Solution for a single order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSolution {
    /// ID of the order this solution is for.
    pub order_id: String,
    /// Status of the solution.
    pub status: OrderStatus,
    /// The route found (if successful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
    /// Actual input amount.
    pub amount_in: BigUint,
    /// Actual output amount.
    pub amount_out: BigUint,
    /// Estimated gas for this order's swaps.
    pub gas_estimate: BigUint,
    /// Price impact in basis points (if calculable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_impact_bps: Option<u16>,
    /// Algorithm that found this solution (internal, not exposed to API).
    #[serde(skip)]
    pub algorithm: String,
}

/// Status of an order solution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Successfully found a route.
    Success,
    /// No route exists between the tokens.
    NoRouteFound,
    /// Route exists but liquidity is insufficient.
    InsufficientLiquidity,
    /// Solving timed out before finding a route.
    Timeout,
}

/// A route consisting of one or more swaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Ordered sequence of swaps to execute.
    pub swaps: Vec<Swap>,
}

impl Route {
    /// Creates a new route from a list of swaps.
    pub fn new(swaps: Vec<Swap>) -> Self {
        Self { swaps }
    }

    /// Returns the number of hops in this route.
    pub fn hop_count(&self) -> usize {
        self.swaps.len()
    }

    /// Returns the input token of the route.
    pub fn input_token(&self) -> Option<Address> {
        self.swaps
            .first()
            .map(|s| s.token_in.clone())
    }

    /// Returns the output token of the route.
    pub fn output_token(&self) -> Option<Address> {
        self.swaps
            .last()
            .map(|s| s.token_out.clone())
    }

    /// Returns all intermediate tokens in the route.
    pub fn intermediate_tokens(&self) -> Vec<Address> {
        if self.swaps.len() <= 1 {
            return vec![];
        }

        self.swaps[..self.swaps.len() - 1]
            .iter()
            .map(|s| s.token_out.clone())
            .collect()
    }

    /// Returns the total gas estimate for this route.
    pub fn total_gas(&self) -> BigUint {
        self.swaps
            .iter()
            .map(|s| &s.gas_estimate)
            .fold(BigUint::ZERO, |acc, g| acc + g)
    }

    /// Validates the route structure.
    pub fn validate(&self) -> Result<(), RouteValidationError> {
        if self.swaps.is_empty() {
            return Err(RouteValidationError::EmptyRoute);
        }

        // Check that consecutive swaps are connected
        for window in self.swaps.windows(2) {
            if window[0].token_out != window[1].token_in {
                return Err(RouteValidationError::DisconnectedSwaps {
                    first_out: window[0].token_out.clone(),
                    second_in: window[1].token_in.clone(),
                });
            }
        }

        Ok(())
    }
}

/// A single swap in a route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swap {
    /// Component to execute the swap on.
    pub component_id: ComponentId,
    /// Protocol system of the component.
    pub protocol: ProtocolSystem,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Amount of input token.
    pub amount_in: BigUint,
    /// Amount of output token.
    pub amount_out: BigUint,
    /// Estimated gas for this swap.
    pub gas_estimate: BigUint,
}

impl Swap {
    /// Creates a new swap.
    pub fn new(
        component_id: ComponentId,
        protocol: ProtocolSystem,
        token_in: Address,
        token_out: Address,
        amount_in: BigUint,
        amount_out: BigUint,
    ) -> Self {
        let gas_estimate = BigUint::from(protocol.typical_gas_cost());
        Self { component_id, protocol, token_in, token_out, amount_in, amount_out, gas_estimate }
    }
}

/// Route validation errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RouteValidationError {
    #[error("route has no swaps")]
    EmptyRoute,
    #[error("swaps are not connected: first outputs {first_out}, second inputs {second_in}")]
    DisconnectedSwaps { first_out: Address, second_in: Address },
}
