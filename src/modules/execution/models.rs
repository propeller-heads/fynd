use tycho_execution::encoding::models::Solution;

use crate::models::{Order, Route};

/// Extension trait for Solution to add construction methods
pub trait SolutionExt {
    /// Create Solution from order and route pair
    fn from_order_route_pair(order: &Order, route: &Route) -> Result<Solution, SolutionExtError>;
}

/// Error types for Solution extension operations
#[derive(Debug)]
pub enum SolutionExtError {
    TokenMismatch,
    MissingAmountOut,
    MissingAmountIn,
}

impl std::fmt::Display for SolutionExtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenMismatch => write!(f, "Order and Route token mismatch"),
            Self::MissingAmountOut => write!(f, "exact_out order missing amount_out"),
            Self::MissingAmountIn => write!(f, "exact_in order missing amount_in"),
        }
    }
}

impl std::error::Error for SolutionExtError {}

impl SolutionExt for Solution {
    fn from_order_route_pair(order: &Order, route: &Route) -> Result<Solution, SolutionExtError> {
        // Validate that order and route are compatible
        if order.token_in().address != route.token_in().address ||
            order.token_out().address != route.token_out().address
        {
            return Err(SolutionExtError::TokenMismatch);
        }

        let solution = Solution {
            sender: order.origin_address().clone(),
            receiver: order
                .receiver()
                .clone()
                .unwrap_or(order.origin_address().clone()),
            exact_out: order.exact_out(),

            given_token: if order.exact_out() {
                route.token_out().address.clone() // Selling token_out to get token_in
            } else {
                order.token_in().address.clone() // Selling token_in to get token_out
            },
            given_amount: if order.exact_out() {
                order
                    .amount_out()
                    .clone()
                    .ok_or(SolutionExtError::MissingAmountOut)?
            } else {
                order
                    .amount_in()
                    .clone()
                    .ok_or(SolutionExtError::MissingAmountIn)?
            },
            checked_token: if order.exact_out() {
                order.token_in().address.clone()
            } else {
                route.token_out().address.clone()
            },
            checked_amount: order.min_amount().clone(), // User's minimum acceptable amount
            swaps: route.swaps().clone(),               /* TODO: for RFQs we need to fill the
                                                         * estimated_amount_in value :/ */
            // we need to check if:
            // - the token in is ETH and the token in of the first swap is WETH -> then wrap
            // - the token out is ETH and the token out of the last swap is WETH -> then unwrap
            native_action: None,
        };

        Ok(solution)
    }
}
