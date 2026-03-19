//! API error types and error response handling.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use fynd_core::SolveError;
pub use fynd_rpc_types::ErrorResponse;
use tracing::warn;

/// API error type that converts to HTTP responses.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Invalid request format or parameters.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Solve operation failed.
    #[error("solve failed: {0}")]
    SolveFailed(#[from] SolveError),

    /// Queue is full, try again later.
    #[error("service overloaded, try again later")]
    ServiceOverloaded,

    /// Internal server error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Market data is stale.
    #[error("market data stale: last update {age_ms}ms ago")]
    StaleData { age_ms: u64 },
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::SolveFailed(e) => match e {
                SolveError::QueueFull => StatusCode::SERVICE_UNAVAILABLE,
                SolveError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            },
            ApiError::ServiceOverloaded => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::StaleData { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn error_response(&self) -> HttpResponse {
        let code = match self {
            ApiError::BadRequest(_) => "BAD_REQUEST",
            ApiError::SolveFailed(e) => match e {
                SolveError::NoRouteFound { .. } => "NO_ROUTE_FOUND",
                SolveError::InsufficientLiquidity { .. } => "INSUFFICIENT_LIQUIDITY",
                SolveError::Timeout { .. } => "TIMEOUT",
                SolveError::QueueFull => "QUEUE_FULL",
                SolveError::AlgorithmError(_) => "ALGORITHM_ERROR",
                SolveError::MarketDataStale { .. } => "STALE_DATA",
                SolveError::InvalidOrder(_) => "INVALID_ORDER",
                SolveError::Internal(_) => "INTERNAL_ERROR",
                SolveError::NotReady(_) => "NOT_READY",
                SolveError::FailedEncoding(_) => "FAILED_ENCODING",
                other => {
                    warn!(?other, "unhandled SolveError variant");
                    "INTERNAL_ERROR"
                }
            },
            ApiError::ServiceOverloaded => "SERVICE_OVERLOADED",
            ApiError::Internal(_) => "INTERNAL_ERROR",
            ApiError::StaleData { .. } => "STALE_DATA",
        };

        let response = ErrorResponse::new(self.to_string(), code.to_string());

        HttpResponse::build(self.status_code()).json(response)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        ApiError::BadRequest(format!("invalid JSON: {}", err))
    }
}
