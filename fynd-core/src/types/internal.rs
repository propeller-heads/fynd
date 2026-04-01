//! Internal types used for task management and worker communication.

use std::time::Instant;

use num_bigint::BigUint;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{Order, SingleOrderQuote};

/// Unique identifier for a solve task.
pub type TaskId = Uuid;

/// Result type for solve operations.
pub type SolveResult = Result<SingleOrderQuote, SolveError>;

/// A task representing a order request in the queue.
pub struct SolveTask {
    /// Unique identifier for this task.
    id: TaskId,
    /// The order request to process.
    order: Order,
    /// Channel to send the result back.
    response_tx: oneshot::Sender<SolveResult>,
    /// When this task was created.
    created_at: Instant,
}

impl SolveTask {
    /// Creates a new solve task.
    pub fn new(id: TaskId, order: Order, response_tx: oneshot::Sender<SolveResult>) -> Self {
        Self { id, order, response_tx, created_at: Instant::now() }
    }

    /// Returns the task ID.
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Returns the order to process.
    pub fn order(&self) -> &Order {
        &self.order
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
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error)]
pub enum SolveError {
    /// No route found between the tokens.
    #[non_exhaustive]
    #[error("no route found for order {order_id}")]
    NoRouteFound {
        /// ID of the order for which no route was found.
        order_id: String,
    },

    /// Insufficient liquidity for the requested amount.
    #[non_exhaustive]
    #[error("insufficient liquidity: need {required}, have {available}")]
    InsufficientLiquidity {
        /// Amount the user requested.
        required: BigUint,
        /// Maximum amount available in the pool.
        available: BigUint,
    },

    /// Solving timed out.
    #[non_exhaustive]
    #[error("solve timeout after {elapsed_ms}ms")]
    Timeout {
        /// Wall-clock time elapsed before the timeout fired, in milliseconds.
        elapsed_ms: u64,
    },

    /// Algorithm-specific error.
    #[error("algorithm error: {0}")]
    AlgorithmError(String),

    /// Market data is too old.
    #[non_exhaustive]
    #[error("market data stale: last update {age_ms}ms ago")]
    MarketDataStale {
        /// Milliseconds since the last successful market-data update.
        age_ms: u64,
    },

    /// Task queue is full.
    #[error("task queue full")]
    QueueFull,

    /// Order validation failed.
    #[error("invalid order: {0}")]
    InvalidOrder(String),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),

    /// No workers are ready to solve.
    #[error("no workers ready: {0}")]
    NotReady(String),

    /// A required derived data computation failed for the current block.
    ///
    /// Unlike `NotReady` (data hasn't arrived yet), this is a permanent failure for
    /// this block — the data will never arrive. Workers should not be retried until
    /// the next block.
    #[error("computation failed: {0}")]
    ComputationFailed(String),

    /// Error when encoding
    #[error("failed to encode: {0}")]
    FailedEncoding(String),

    /// Price check against external source failed.
    #[error("price check failed for order {order_id}")]
    PriceCheckFailed {
        /// Identifier of the order that failed the price check.
        order_id: String,
    },
}

impl SolveError {
    /// Returns true if this error should be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SolveError::Timeout { .. } | SolveError::MarketDataStale { .. } | SolveError::QueueFull
        )
    }

    /// Creates a [`SolveError::NoRouteFound`] for the given order ID.
    pub fn no_route_found(order_id: impl Into<String>) -> Self {
        Self::NoRouteFound { order_id: order_id.into() }
    }

    /// Creates a [`SolveError::InsufficientLiquidity`] with the required and available amounts.
    pub fn insufficient_liquidity(required: BigUint, available: BigUint) -> Self {
        Self::InsufficientLiquidity { required, available }
    }

    /// Creates a [`SolveError::Timeout`] with the elapsed time in milliseconds.
    pub fn timeout(elapsed_ms: u64) -> Self {
        Self::Timeout { elapsed_ms }
    }

    /// Creates a [`SolveError::MarketDataStale`] with the data age in milliseconds.
    pub fn market_data_stale(age_ms: u64) -> Self {
        Self::MarketDataStale { age_ms }
    }
}
