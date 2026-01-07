//! API request and response types.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// Request to solve one or more swap orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionRequest {
    /// Orders to solve.
    pub orders: Vec<Order>,
    /// Optional solving parameters.
    #[serde(default)]
    pub options: SolutionOptions,
}

/// Options to customize the solving behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SolutionOptions {
    /// Timeout in milliseconds (if None, uses default).
    pub timeout_ms: Option<u64>,
}

/// A single swap order to be solved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Unique identifier for this order.
    pub id: String,
    /// Input token address.
    pub token_in: Address,
    /// Output token address.
    pub token_out: Address,
    /// Amount of input token (for exact-in orders).
    /// Mutually exclusive with `amount_out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_in: Option<U256>,
    /// Amount of output token (for exact-out orders).
    /// Mutually exclusive with `amount_in`.
    /// TODO: Verify if we aim to accept exact-out orders
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_out: Option<U256>,
    /// Address that will send the input tokens.
    pub sender: Address,
    /// Address that will receive the output tokens (if different from sender).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<Address>,
    /// Maximum slippage tolerance in bps (e.g., 50 = 0.5%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<u16>,
}

impl Order {
    /// Returns true if this is an exact-in order.
    pub fn is_exact_in(&self) -> bool {
        self.amount_in.is_some()
    }

    /// Returns true if this is an exact-out order.
    pub fn is_exact_out(&self) -> bool {
        self.amount_out.is_some()
    }

    /// Returns the amount being specified (either in or out).
    pub fn specified_amount(&self) -> Option<U256> {
        self.amount_in.or(self.amount_out)
    }

    /// Returns the effective receiver address.
    pub fn effective_receiver(&self) -> Address {
        self.receiver.unwrap_or(self.sender)
    }

    /// Validates the order structure.
    pub fn validate(&self) -> Result<(), OrderValidationError> {
        // Must have exactly one of amount_in or amount_out
        match (self.amount_in, self.amount_out) {
            (Some(_), Some(_)) => {
                return Err(OrderValidationError::BothAmountsSpecified);
            }
            (None, None) => {
                return Err(OrderValidationError::NoAmountSpecified);
            }
            _ => {}
        }

        // Token addresses must be different
        if self.token_in == self.token_out {
            return Err(OrderValidationError::SameTokens);
        }

        // Amount must be non-zero
        if let Some(amount) = self.specified_amount() {
            if amount.is_zero() {
                return Err(OrderValidationError::ZeroAmount);
            }
        }

        Ok(())
    }
}

/// Order validation errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OrderValidationError {
    #[error("both amount_in and amount_out specified")]
    BothAmountsSpecified,
    #[error("neither amount_in nor amount_out specified")]
    NoAmountSpecified,
    #[error("token_in and token_out are the same")]
    SameTokens,
    #[error("amount is zero")]
    ZeroAmount,
}

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Whether the service is healthy.
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    pub last_update_ms: u64,
    /// Number of pending tasks in queue.
    pub queue_depth: usize,
}
