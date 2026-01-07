//! HTTP request handlers for the router API.

use actix_web::{get, post, web, HttpResponse};
use serde::Serialize;
use tracing::{info, instrument, warn};

use super::{ApiError, AppState};
use crate::types::{HealthStatus, SolutionRequest};

/// Configures API routes.
pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(solve).service(health).service(info);
}

/// POST /solve - Submit a solve request.
///
/// Accepts a `SolutionRequest` and returns a `Solution` with the best routes found.
///
/// # Errors
///
/// - 400 Bad Request: Invalid request format
/// - 422 Unprocessable Entity: No routes found
/// - 503 Service Unavailable: Queue full or service overloaded
/// - 504 Gateway Timeout: Solve timeout
#[post("/solve")]
#[instrument(skip(state, request), fields(num_orders = request.orders.len()))]
pub async fn solve(
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
            return Err(ApiError::BadRequest(format!(
                "invalid order {}: {}",
                order.id, e
            )));
        }
    }

    // Check queue depth
    if state.task_queue.is_full() {
        warn!("task queue full, rejecting request");
        return Err(ApiError::ServiceOverloaded);
    }

    // Enqueue and wait for result
    let solution = state.task_queue.enqueue(request).await?;

    info!(
        solve_time_ms = solution.solve_time_ms,
        num_orders = solution.orders.len(),
        "solve completed"
    );

    Ok(HttpResponse::Ok().json(solution))
}

/// GET /health - Health check endpoint.
///
/// Returns the current health status of the service.
#[get("/health")]
pub async fn health(state: web::Data<AppState>) -> HttpResponse {
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

/// Response for the /info endpoint.
#[derive(Serialize)]
pub struct InfoResponse {
    pub name: &'static str,
    pub version: &'static str,
    pub algorithms: Vec<&'static str>,
}

/// GET /info - Service information endpoint.
///
/// Returns information about the service.
#[get("/info")]
pub async fn info() -> HttpResponse {
    let info = InfoResponse {
        name: "tycho-router",
        version: env!("CARGO_PKG_VERSION"),
        algorithms: vec!["most_liquid"],
    };

    HttpResponse::Ok().json(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};

    // TODO: Add integration tests for handlers
}
