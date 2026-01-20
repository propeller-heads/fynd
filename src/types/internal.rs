//! Internal types used for task management and worker communication.

use std::time::Instant;

use num_bigint::BigUint;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{Order, SingleOrderSolution};

/// Unique identifier for a solve task.
pub type TaskId = Uuid;

/// Result type for solve operations.
pub type SolveResult = Result<SingleOrderSolution, SolveError>;

/// A task representing a order request in the queue.
pub struct SolveTask {
    /// Unique identifier for this task.
    pub id: TaskId,
    /// The order request to process.
    pub order: Order,
    /// Channel to send the result back.
    pub response_tx: oneshot::Sender<SolveResult>,
    /// When this task was created.
    pub created_at: Instant,
}

impl SolveTask {
    /// Creates a new solve task.
    pub fn new(id: TaskId, order: Order, response_tx: oneshot::Sender<SolveResult>) -> Self {
        Self { id, order, response_tx, created_at: Instant::now() }
    }

    /// Returns how long this task has been waiting.
    pub fn wait_time(&self) -> std::time::Duration {
        self.created_at.elapsed()
    }

    /// Sends the result back to the requester.
    /// Consumes self because oneshot::Sender can only be used once.
    pub fn respond(self, result: SolveResult) {
        // Ignore error if receiver was dropped
        let _ = self.response_tx.send(result);
    }
}

/// Errors that can occur during solving.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SolveError {
    /// No route found between the tokens.
    #[error("no route found for order {order_id}")]
    NoRouteFound { order_id: String },

    /// Insufficient liquidity for the requested amount.
    #[error("insufficient liquidity: need {required}, have {available}")]
    InsufficientLiquidity { required: BigUint, available: BigUint },

    /// Solving timed out.
    #[error("solve timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// Algorithm-specific error.
    #[error("algorithm error: {0}")]
    AlgorithmError(String),

    /// Market data is too old.
    #[error("market data stale: last update {age_ms}ms ago")]
    MarketDataStale { age_ms: u64 },

    /// Task queue is full.
    #[error("task queue full")]
    QueueFull,

    /// Order validation failed.
    #[error("invalid order: {0}")]
    InvalidOrder(String),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl SolveError {
    /// Returns true if this error should be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SolveError::Timeout { .. } | SolveError::MarketDataStale { .. } | SolveError::QueueFull
        )
    }
}
