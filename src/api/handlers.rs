//! HTTP request handlers for the router API.

use actix_web::{web, HttpResponse};
use tracing::{info, instrument, warn};

use super::{ApiError, AppState};
use crate::types::{solution::SolutionRequest, HealthStatus};

/// Configures API routes under /v1 namespace.
pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/v1")
            .route("/solve", web::post().to(solve))
            .route("/health", web::get().to(health)),
    );
}

/// POST /v1/solve - Submit a solve request.
///
/// Accepts a `SolutionRequest` and returns a `Solution` with the best routes found, or an error
/// if the request could not be filled.
///
/// # Errors
///
/// - 400 Bad Request: Invalid request format
/// - 422 Unprocessable Entity: No routes found
/// - 503 Service Unavailable: Queue full or service overloaded
/// - 504 Gateway Timeout: Solve timeout
#[instrument(skip(state, request), fields(num_orders = request.orders.len()))]
async fn solve(
    state: web::Data<AppState>,
    request: web::Json<SolutionRequest>,
) -> Result<HttpResponse, ApiError> {
    let request = request.into_inner();

    // Validate request
    if request.orders.is_empty() {
        return Err(ApiError::BadRequest("no orders provided".to_string()));
    }

    for order in &request.orders {
        if let Err(e) = order.validate() {
            return Err(ApiError::BadRequest(format!("invalid order {}: {}", order.id, e)));
        }
    }

    // Check queue depth
    if state.task_queue.is_full() {
        warn!("task queue full, rejecting request");
        return Err(ApiError::ServiceOverloaded);
    }

    // Enqueue and wait for result
    let solution = state
        .task_queue
        .enqueue(request)
        .await?;

    info!(
        solve_time_ms = solution.solve_time_ms,
        num_orders = solution.orders.len(),
        "solve completed"
    );

    Ok(HttpResponse::Ok().json(solution))
}

/// GET /v1/health - Health check endpoint.
///
/// Returns the current health status of the service.
async fn health(state: web::Data<AppState>) -> HttpResponse {
    let age_ms = state.health_tracker.age_ms();
    let is_healthy = age_ms < 60_000; // Healthy if data less than 60s old

    let status = HealthStatus {
        healthy: is_healthy,
        last_update_ms: age_ms,
        queue_depth: state.task_queue.approximate_depth(),
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
