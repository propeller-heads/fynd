//! HTTP API layer using Actix Web.
//!
//! This module provides the HTTP endpoints for the solver:
//! - POST /quote - Submit solve requests
//! - GET /health - Health check endpoint

pub mod dto;
pub mod error;
pub mod handlers;
#[cfg(feature = "experimental")]
pub mod prices;

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use actix_web::{web, HttpResponse, ResponseError};
pub use dto::HealthStatus;
pub use error::ApiError;
use fynd_core::{
    derived::SharedDerivedDataRef, feed::market_data::SharedMarketDataRef,
    worker_pool_router::WorkerPoolRouter,
};
use handlers::configure_routes;
#[cfg(feature = "experimental")]
use tycho_simulation::tycho_common::models::Address;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::error::ErrorResponse;

#[derive(OpenApi)]
#[openapi(
    paths(handlers::quote, handlers::health,),
    components(schemas(
        dto::QuoteRequest,
        dto::Order,
        dto::OrderSide,
        dto::QuoteOptions,
        dto::Quote,
        dto::OrderQuote,
        dto::QuoteStatus,
        dto::Route,
        dto::Swap,
        dto::BlockInfo,
        HealthStatus,
        ErrorResponse,
    ))
)]
pub struct ApiDoc;

#[cfg(feature = "experimental")]
#[derive(OpenApi)]
#[openapi(
    paths(handlers::get_prices),
    components(schemas(
        prices::PricesResponse,
        prices::TokenPriceEntry,
        prices::SpotPriceEntry,
        prices::PoolDepthEntry,
    ))
)]
pub struct ExperimentalApiDoc;

/// Simple tracker for service health metrics.
///
/// Reads the last update timestamp from SharedMarketData to determine how fresh the market data is,
/// and checks derived data overall readiness.
#[derive(Clone)]
pub(crate) struct HealthTracker {
    market_data: SharedMarketDataRef,
    derived_data: SharedDerivedDataRef,
    gas_price_stale_threshold: Option<Duration>,
    created_at: Instant,
}

impl HealthTracker {
    /// Creates a new health tracker.
    pub(crate) fn new(
        market_data: SharedMarketDataRef,
        derived_data: SharedDerivedDataRef,
    ) -> Self {
        Self {
            market_data,
            derived_data,
            gas_price_stale_threshold: None,
            created_at: Instant::now(),
        }
    }

    /// Sets the gas price staleness threshold. Health returns 503 when exceeded.
    pub(crate) fn with_gas_price_stale_threshold(mut self, threshold: Option<Duration>) -> Self {
        self.gas_price_stale_threshold = threshold;
        self
    }

    /// Returns milliseconds since the last market data update.
    pub(crate) async fn age_ms(&self) -> u64 {
        let data = self.market_data.read().await;
        match data.last_updated() {
            Some(block_info) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                // Convert block timestamp (seconds) to ms and calculate age
                now.saturating_sub(block_info.timestamp())
                    .saturating_mul(1000)
            }
            None => u64::MAX, // Never updated
        }
    }

    /// Returns milliseconds since the last gas price update, if available.
    pub(crate) async fn gas_price_age_ms(&self) -> Option<u64> {
        let data = self.market_data.read().await;
        let gas_price = data.gas_price()?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let block_ms = gas_price
            .block_timestamp
            .saturating_mul(1000);
        Some(now_ms.saturating_sub(block_ms))
    }

    /// Returns whether the gas price is stale according to the configured threshold.
    ///
    /// During startup (before `threshold` has elapsed), a missing gas price is not
    /// considered stale — the first fetch may not have completed yet.
    pub(crate) async fn gas_price_stale(&self) -> bool {
        let Some(threshold) = self.gas_price_stale_threshold else { return false };
        match self.gas_price_age_ms().await {
            Some(age_ms) => age_ms > threshold.as_millis() as u64,
            None => self.created_at.elapsed() > threshold,
        }
    }

    /// Returns whether derived data has been computed at least once.
    ///
    /// This checks overall readiness (has any computation cycle completed), not per-block
    /// freshness. Algorithms that require fresh derived data are ready to receive orders but
    /// will wait for per-block recomputation before solving.
    pub(crate) async fn derived_data_ready(&self) -> bool {
        self.derived_data
            .read()
            .await
            .last_block()
            .is_some()
    }
}

/// Shared application state for HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    worker_router: Arc<WorkerPoolRouter>,
    health_tracker: HealthTracker,
    #[cfg(feature = "experimental")]
    pub(crate) derived_data: SharedDerivedDataRef,
    #[cfg(feature = "experimental")]
    pub(crate) gas_token: Address,
}

impl AppState {
    /// Creates new application state.
    pub(crate) fn new(
        worker_router: WorkerPoolRouter,
        health_tracker: HealthTracker,
        #[cfg(feature = "experimental")] derived_data: SharedDerivedDataRef,
        #[cfg(feature = "experimental")] gas_token: Address,
    ) -> Self {
        Self {
            worker_router: Arc::new(worker_router),
            health_tracker,
            #[cfg(feature = "experimental")]
            derived_data,
            #[cfg(feature = "experimental")]
            gas_token,
        }
    }

    pub(crate) fn worker_router(&self) -> &Arc<WorkerPoolRouter> {
        &self.worker_router
    }

    pub(crate) fn health_tracker(&self) -> &HealthTracker {
        &self.health_tracker
    }
}

/// Registers JSON and query-string extractor error handlers so that malformed
/// requests always receive a JSON `ErrorResponse` body instead of actix-web's
/// default plain-text response.
pub(crate) fn configure_error_handlers(cfg: &mut web::ServiceConfig) {
    cfg.app_data(web::JsonConfig::default().error_handler(|err, _req| {
        let api_err = ApiError::BadRequest(format!("invalid JSON: {err}"));
        actix_web::error::InternalError::from_response(err, api_err.error_response()).into()
    }))
    .app_data(web::QueryConfig::default().error_handler(|err, _req| {
        let api_err = ApiError::BadRequest(format!("invalid query parameter: {err}"));
        actix_web::error::InternalError::from_response(err, api_err.error_response()).into()
    }));
}

/// Configures the Actix Web application with routes and state.
pub(crate) fn configure_app(cfg: &mut web::ServiceConfig, state: AppState) {
    #[allow(unused_mut)]
    let mut openapi = ApiDoc::openapi();
    #[cfg(feature = "experimental")]
    {
        let experimental = ExperimentalApiDoc::openapi();
        openapi.merge(experimental);
    }
    cfg.configure(configure_error_handlers)
        .app_data(web::Data::new(state))
        .configure(configure_routes)
        .service(SwaggerUi::new("/docs/{_:.*}").url("/api-docs/openapi.json", openapi))
        .default_service(web::to(|| async {
            let body = ErrorResponse::new("not found".into(), "NOT_FOUND".into());
            HttpResponse::NotFound().json(body)
        }));
}
