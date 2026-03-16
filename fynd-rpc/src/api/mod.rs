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
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::web;
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
pub struct HealthTracker {
    market_data: SharedMarketDataRef,
    derived_data: SharedDerivedDataRef,
}

impl HealthTracker {
    /// Creates a new health tracker.
    pub fn new(market_data: SharedMarketDataRef, derived_data: SharedDerivedDataRef) -> Self {
        Self { market_data, derived_data }
    }

    /// Returns milliseconds since the last market data update.
    pub async fn age_ms(&self) -> u64 {
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

    /// Returns whether derived data has been computed at least once.
    ///
    /// This checks overall readiness (has any computation cycle completed), not per-block
    /// freshness. Algorithms that require fresh derived data are ready to receive orders but
    /// will wait for per-block recomputation before solving.
    pub async fn derived_data_ready(&self) -> bool {
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
    /// WorkerPoolRouter for solving requests across multiple solver pools.
    pub worker_router: Arc<WorkerPoolRouter>,
    /// Health tracker for monitoring data freshness.
    pub health_tracker: HealthTracker,
    /// Shared derived data (token prices, spot prices, pool depths).
    #[cfg(feature = "experimental")]
    pub derived_data: SharedDerivedDataRef,
    /// Gas token address for this chain (e.g. WETH).
    #[cfg(feature = "experimental")]
    pub gas_token: Address,
}

impl AppState {
    /// Creates new application state.
    pub fn new(
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
}

/// Configures the Actix Web application with routes and state.
pub fn configure_app(cfg: &mut web::ServiceConfig, state: AppState) {
    #[allow(unused_mut)]
    let mut openapi = ApiDoc::openapi();
    #[cfg(feature = "experimental")]
    {
        let experimental = ExperimentalApiDoc::openapi();
        openapi.merge(experimental);
    }
    cfg.app_data(web::Data::new(state))
        .configure(configure_routes)
        .service(SwaggerUi::new("/docs/{_:.*}").url("/api-docs/openapi.json", openapi));
}
