//! HTTP request handlers for the solver API.

use actix_web::{web, HttpResponse};
#[cfg(feature = "experimental")]
use tracing::warn;
use tracing::{info, instrument};

use super::{dto, ApiError, AppState};
use crate::api::error::ErrorResponse;
#[cfg(feature = "experimental")]
use crate::api::prices::{
    price_to_f64, IncludeField, PoolDepthEntry, PricesQuery, PricesResponse, SpotPriceEntry,
    TokenPriceEntry,
};

/// Configures API routes under /v1 namespace.
pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    let scope = web::scope("/v1")
        .route("/quote", web::post().to(quote))
        .route("/health", web::get().to(health));
    #[cfg(feature = "experimental")]
    let scope = scope.route("/prices", web::get().to(get_prices));
    cfg.service(scope);
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
        .worker_router
        .quote(core_request)
        .await?;

    info!(
        solve_time_ms = core_quote.solve_time_ms(),
        num_orders = core_quote.orders().len(),
        num_pools = state.worker_router.num_pools(),
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
        (status = 200, description = "Service healthy", body = dto::HealthStatus),
        (status = 503, description = "Data stale", body = dto::HealthStatus),
    )
)]
pub async fn health(state: web::Data<AppState>) -> HttpResponse {
    let age_ms = state.health_tracker.age_ms().await;
    let data_fresh = age_ms < 60_000; // Healthy if data less than 60s old
    let derived_data_ready = state
        .health_tracker
        .derived_data_ready()
        .await;
    let gas_price_age_ms = state
        .health_tracker
        .gas_price_age_ms()
        .await;
    let gas_stale = state
        .health_tracker
        .gas_price_stale()
        .await;
    let is_healthy = data_fresh && derived_data_ready && !gas_stale;

    let status = dto::HealthStatus {
        healthy: is_healthy,
        last_update_ms: age_ms,
        num_solver_pools: state.worker_router.num_pools(),
        derived_data_ready,
        gas_price_age_ms,
    };

    if is_healthy {
        HttpResponse::Ok().json(status)
    } else {
        HttpResponse::ServiceUnavailable().json(status)
    }
}

#[cfg(feature = "experimental")]
/// Default limit for spot_prices and pool_depths entries.
const DEFAULT_PRICES_LIMIT: usize = 1000;

#[cfg(feature = "experimental")]
/// GET /v1/prices - Return derived token prices and optional market data.
///
/// By default returns token gas prices only. Use `include` query parameter
/// to add spot prices and/or pool depths.
///
/// # Query Parameters
///
/// - `include` - Comma-separated list: `depths`, `spot_prices`
/// - `limit` - Max entries for spot_prices / pool_depths (default: 1000)
#[utoipa::path(
    get,
    path = "/v1/prices",
    tag = "prices",
    params(PricesQuery),
    responses(
        (status = 200, description = "Prices returned", body = PricesResponse),
        (status = 400, description = "Invalid query parameter", body = ErrorResponse),
        (status = 503, description = "Data not yet available", body = ErrorResponse),
    )
)]
#[instrument(skip(state))]
pub async fn get_prices(
    state: web::Data<AppState>,
    query: web::Query<PricesQuery>,
) -> Result<HttpResponse, ApiError> {
    // Parse include fields (reject unknowns with 400)
    let include_fields = match &query.include {
        Some(raw) => IncludeField::parse_include(raw).map_err(ApiError::BadRequest)?,
        None => vec![],
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PRICES_LIMIT);
    let want_depths = include_fields.contains(&IncludeField::Depths);
    let want_spot = include_fields.contains(&IncludeField::SpotPrices);

    // Acquire read lock, check staleness first (avoid cloning if 503), then clone
    let store = state.derived_data.read().await;
    let last_block = store
        .last_block()
        .ok_or(ApiError::StaleData { age_ms: u64::MAX })?;
    let token_prices = store.token_prices().cloned();
    let spot_prices_data = if want_spot { store.spot_prices().cloned() } else { None };
    let pool_depths_data = if want_depths { store.pool_depths().cloned() } else { None };
    drop(store);

    // Convert token gas prices
    let mut prices = Vec::new();
    if let Some(token_prices) = &token_prices {
        for (address, price) in token_prices {
            match price_to_f64(&price.numerator, &price.denominator) {
                Some(f) => {
                    prices.push(TokenPriceEntry { token: address.clone(), price: f });
                }
                None => {
                    warn!(
                        token = %address,
                        "skipping token with unconvertible price (zero denom or overflow)"
                    );
                }
            }
        }
    }
    // Convert spot prices if requested (sorted for deterministic limit)
    let spot_prices = if want_spot {
        let mut entries: Vec<SpotPriceEntry> = spot_prices_data
            .into_iter()
            .flatten()
            .map(|((component_id, token_in, token_out), price)| SpotPriceEntry {
                component_id,
                token_in,
                token_out,
                price,
            })
            .collect();
        entries.sort_by(|a, b| {
            (&a.component_id, &a.token_in, &a.token_out).cmp(&(
                &b.component_id,
                &b.token_in,
                &b.token_out,
            ))
        });
        entries.truncate(limit);
        Some(entries)
    } else {
        None
    };

    // Convert pool depths if requested (sorted for deterministic limit)
    let pool_depths = if want_depths {
        let mut entries: Vec<PoolDepthEntry> = pool_depths_data
            .into_iter()
            .flatten()
            .map(|((component_id, token_in, token_out), depth)| PoolDepthEntry {
                component_id,
                token_in,
                token_out,
                depth: depth.to_string(),
            })
            .collect();
        entries.sort_by(|a, b| {
            (&a.component_id, &a.token_in, &a.token_out).cmp(&(
                &b.component_id,
                &b.token_in,
                &b.token_out,
            ))
        });
        entries.truncate(limit);
        Some(entries)
    } else {
        None
    };

    let response = PricesResponse {
        prices,
        gas_token: state.gas_token.clone(),
        last_block,
        spot_prices,
        pool_depths,
    };

    info!(
        num_tokens = response.prices.len(),
        has_spot = response.spot_prices.is_some(),
        has_depths = response.pool_depths.is_some(),
        "prices response"
    );

    Ok(HttpResponse::Ok().json(response))
}

#[cfg(test)]
mod tests {
    // TODO: Add integration tests for handlers
}
