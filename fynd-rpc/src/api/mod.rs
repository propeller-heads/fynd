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
#[cfg(feature = "experimental")]
use fynd_core::derived::SharedDerivedDataRef;
use fynd_core::{feed::market_data::SharedMarketDataRef, order_manager::OrderManager};
use handlers::configure_routes;
#[cfg(feature = "experimental")]
use tycho_simulation::tycho_common::models::Address;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::error::ErrorResponse;

#[derive(OpenApi)]
#[openapi(
    paths(
        handlers::quote,
        handlers::health,
    ),
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
/// Reads the last update timestamp from SharedMarketData to determine
/// how fresh the market data is.
#[derive(Clone)]
pub struct HealthTracker {
    market_data: SharedMarketDataRef,
}

impl HealthTracker {
    /// Creates a new health tracker.
    pub fn new(market_data: SharedMarketDataRef) -> Self {
        Self { market_data }
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
}

/// Shared application state for HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    /// OrderManager for solving requests across multiple solver pools.
    pub order_manager: Arc<OrderManager>,
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
        order_manager: OrderManager,
        health_tracker: HealthTracker,
        #[cfg(feature = "experimental")] derived_data: SharedDerivedDataRef,
        #[cfg(feature = "experimental")] gas_token: Address,
    ) -> Self {
        Self {
            order_manager: Arc::new(order_manager),
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
