//! HTTP request handlers for the solver API.

use actix_web::{web, HttpResponse};
use tracing::{info, instrument};

use super::{dto, ApiError, AppState};
use crate::api::{dto::HealthStatus, error::ErrorResponse};

/// Configures API routes under /v1 namespace.
pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/v1")
            .route("/quote", web::post().to(quote))
            .route("/health", web::get().to(health)),
    );
}

/// POST /v1/quote - Request a quote.
///
/// Accepts a `QuoteRequest` and returns a `Quote` with the best routes found, or an error
/// if the request could not be filled.
///
/// # Errors
///
/// - 400 Bad Request: Invalid request format
/// - 422 Unprocessable Entity: No routes found
/// - 503 Service Unavailable: Queue full or service overloaded
/// - 504 Gateway Timeout: Quote timeout
#[utoipa::path(
    post,
    path = "/v1/quote",
    tag = "solver",
    request_body = dto::QuoteRequest,
    responses(
        (status = 200, description = "Quote completed", body = dto::Quote),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 422, description = "No route found", body = ErrorResponse),
        (status = 503, description = "Service unavailable", body = ErrorResponse),
        (status = 504, description = "Quote timeout", body = ErrorResponse),
    )
)]
#[instrument(skip(state, request), fields(num_orders = request.orders.len()))]
pub async fn quote(
    state: web::Data<AppState>,
    request: web::Json<dto::QuoteRequest>,
) -> Result<HttpResponse, ApiError> {
    let dto_request = request.into_inner();

    // Validate request
    if dto_request.orders.is_empty() {
        return Err(ApiError::BadRequest("no orders provided".to_string()));
    }

    // Convert DTO to core types
    let core_request: fynd_core::QuoteRequest = dto_request.into();

    // Validate orders
    for order in core_request.orders() {
        if let Err(e) = order.validate() {
            return Err(ApiError::BadRequest(format!("invalid order {}: {}", order.id(), e)));
        }
    }

    let core_quote = state
        .order_manager
        .quote(core_request)
        .await?;

    info!(
        solve_time_ms = core_quote.solve_time_ms(),
        num_orders = core_quote.orders().len(),
        num_pools = state.order_manager.num_pools(),
        "quote completed"
    );

    let dto_quote: dto::Quote = core_quote.into();

    Ok(HttpResponse::Ok().json(dto_quote))
}

/// GET /v1/health - Health check endpoint.
///
/// Returns the current health status of the service.
#[utoipa::path(
    get,
    path = "/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "Service healthy", body = HealthStatus),
        (status = 503, description = "Data stale", body = HealthStatus),
    )
)]
pub async fn health(state: web::Data<AppState>) -> HttpResponse {
    let age_ms = state.health_tracker.age_ms().await;
    let is_healthy = age_ms < 60_000; // Healthy if data less than 60s old

    let status = HealthStatus {
        healthy: is_healthy,
        last_update_ms: age_ms,
        num_solver_pools: state.order_manager.num_pools(),
    };

    if is_healthy {
        HttpResponse::Ok().json(status)
    } else {
        HttpResponse::ServiceUnavailable().json(status)
    }
}

#[cfg(test)]
mod tests {
    // TODO: Add integration tests for handlers
}
