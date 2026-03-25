//! API error types and error response handling.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use fynd_core::SolveError;
pub use fynd_rpc_types::ErrorResponse;
use tracing::warn;

/// API error type that converts to HTTP responses.
#[non_exhaustive]
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
    #[non_exhaustive]
    StaleData { age_ms: u64 },
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::SolveFailed(e) => match e {
                SolveError::QueueFull => StatusCode::SERVICE_UNAVAILABLE,
                SolveError::Timeout { .. } => StatusCode::SERVICE_UNAVAILABLE,
                SolveError::MarketDataStale { .. } => StatusCode::SERVICE_UNAVAILABLE,
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

#[cfg(test)]
mod tests {
    use actix_web::{body::to_bytes, http::StatusCode, ResponseError};
    use fynd_core::SolveError;
    use num_bigint::BigUint;
    use serde_json::Value;

    use super::ApiError;

    async fn json_body(err: ApiError) -> (StatusCode, Value) {
        let status = err.status_code();
        let resp = err.error_response();
        let bytes = to_bytes(resp.into_body())
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        (status, body)
    }

    #[actix_web::test]
    async fn test_bad_request() {
        let (status, body) = json_body(ApiError::BadRequest("missing field".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    #[actix_web::test]
    async fn test_no_route_found() {
        let (status, body) =
            json_body(ApiError::SolveFailed(SolveError::no_route_found("order-1"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "NO_ROUTE_FOUND");
    }

    #[actix_web::test]
    async fn test_insufficient_liquidity() {
        let err = SolveError::insufficient_liquidity(BigUint::from(100u64), BigUint::from(50u64));
        let (status, body) = json_body(ApiError::SolveFailed(err)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "INSUFFICIENT_LIQUIDITY");
    }

    #[actix_web::test]
    async fn test_timeout() {
        let (status, body) = json_body(ApiError::SolveFailed(SolveError::timeout(100))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "TIMEOUT");
    }

    #[actix_web::test]
    async fn test_queue_full() {
        let (status, body) = json_body(ApiError::SolveFailed(SolveError::QueueFull)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "QUEUE_FULL");
    }

    #[actix_web::test]
    async fn test_service_overloaded() {
        let (status, body) = json_body(ApiError::ServiceOverloaded).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "SERVICE_OVERLOADED");
    }

    #[actix_web::test]
    async fn test_internal_error() {
        let (status, body) = json_body(ApiError::Internal("db down".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["code"], "INTERNAL_ERROR");
    }

    #[actix_web::test]
    async fn test_stale_data() {
        let (status, body) = json_body(ApiError::StaleData { age_ms: 90_000 }).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "STALE_DATA");
    }

    #[actix_web::test]
    async fn test_market_data_stale_via_solve_failed() {
        let err = SolveError::market_data_stale(5_000);
        let (status, body) = json_body(ApiError::SolveFailed(err)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "STALE_DATA");
    }
}
