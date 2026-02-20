//! HTTP API layer using Actix Web.
//!
//! This module provides the HTTP endpoints for the solver:
//! - POST /solve - Submit solve requests
//! - GET /health - Health check endpoint

pub mod error;
pub mod handlers;
pub mod types;

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::web;
pub use error::ApiError;
use fynd_core::{
    feed::market_data::SharedMarketDataRef, BlockInfo, Order, OrderSide, OrderSolution, Route,
    SolutionStatus, Swap,
};
use handlers::configure_routes;
pub use types::{HealthStatus, Solution, SolutionOptions, SolutionRequest};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::{api::error::ErrorResponse, order_manager::OrderManager};

#[derive(OpenApi)]
#[openapi(
    paths(handlers::solve, handlers::health),
    components(schemas(
        SolutionRequest,
        Order,
        OrderSide,
        SolutionOptions,
        Solution,
        OrderSolution,
        SolutionStatus,
        Route,
        Swap,
        BlockInfo,
        HealthStatus,
        ErrorResponse,
    ))
)]
pub struct ApiDoc;

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
                now.saturating_sub(block_info.timestamp)
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
}

impl AppState {
    /// Creates new application state.
    pub fn new(order_manager: OrderManager, health_tracker: HealthTracker) -> Self {
        Self { order_manager: Arc::new(order_manager), health_tracker }
    }
}

/// Configures the Actix Web application with routes and state.
pub fn configure_app(cfg: &mut web::ServiceConfig, state: AppState) {
    cfg.app_data(web::Data::new(state))
        .configure(configure_routes)
        .service(SwaggerUi::new("/docs/{_:.*}").url("/api-docs/openapi.json", ApiDoc::openapi()));
}
