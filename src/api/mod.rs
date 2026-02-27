//! HTTP API layer using Actix Web.
//!
//! This module provides the HTTP endpoints for the solver:
//! - POST /solve - Submit solve requests
//! - GET /health - Health check endpoint

pub mod error;
pub mod handlers;

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::web;
pub use error::ApiError;
use handlers::configure_routes;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    api::error::ErrorResponse,
    order_manager::OrderManager,
    types::{
        solution::{
            BlockInfo, EncodedSwapData, EncodingOptions, Order, OrderSide, OrderSolution, Route,
            Solution, SolutionOptions, SolutionRequest, SolutionStatus, Swap, TransferType,
        },
        HealthStatus,
    },
};

#[derive(OpenApi)]
#[openapi(
    paths(handlers::solve, handlers::health),
    components(schemas(
        SolutionRequest,
        Order,
        OrderSide,
        SolutionOptions,
        EncodingOptions,
        TransferType,
        Solution,
        OrderSolution,
        SolutionStatus,
        Route,
        Swap,
        BlockInfo,
        EncodedSwapData,
        HealthStatus,
        ErrorResponse,
    ))
)]
pub struct ApiDoc;

/// Simple tracker for service health metrics.
///
/// This is a lightweight alternative to passing SharedMarketDataRef
/// to the API layer. TychoFeed calls `update()` when it receives
/// new data, and the health handler reads `age_ms()`.
#[derive(Clone)]
pub struct HealthTracker {
    last_update_ms: Arc<AtomicU64>,
}

impl HealthTracker {
    /// Creates a new health tracker.
    pub fn new() -> Self {
        Self { last_update_ms: Arc::new(AtomicU64::new(0)) }
    }

    /// Updates the last update timestamp to now.
    /// Called by TychoFeed when market data is updated.
    pub fn update(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.last_update_ms
            .store(now, Ordering::Relaxed);
    }

    /// Returns milliseconds since the last update.
    pub fn age_ms(&self) -> u64 {
        let last = self
            .last_update_ms
            .load(Ordering::Relaxed);
        if last == 0 {
            return u64::MAX; // Never updated
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        now.saturating_sub(last)
    }
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new()
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
