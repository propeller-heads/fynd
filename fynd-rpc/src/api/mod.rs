//! HTTP API layer: endpoint handlers, OpenAPI docs, and shared application state.

/// OpenAPI spec construction and Swagger UI registration.
mod docs;
/// Re-exports of wire-format DTO types from `fynd-rpc-types`.
pub mod dto;
/// [`ApiError`] type with HTTP status code mapping.
pub mod error;
/// Resolves the caller's access to exclusive liquidity from the request headers.
pub(crate) mod exclusive_access;
/// Request handlers for `/v1/quote`, `/v1/health`, and `/v1/info`.
pub mod handlers;
/// HTTP metrics middleware recording request duration and per-client usage.
pub(crate) mod middleware;
#[cfg(feature = "experimental")]
/// Response types and handler for `GET /v1/prices` (experimental).
pub mod prices;
/// Builds re-issuable, signature-free representation of a quote request for replay logging.
pub(crate) mod request_capture;
#[cfg(feature = "experimental")]
/// Response types and helpers for `GET /v1/tokens` (experimental).
pub mod tokens;

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use actix_web::{web, HttpResponse, ResponseError};
pub use dto::HealthStatus;
pub use error::ApiError;
use fynd_core::{
    derived::SharedDerivedDataRef, feed::market_data::MarketData,
    worker_pool_router::WorkerPoolRouter,
};
use handlers::configure_routes;
#[cfg(feature = "experimental")]
use tycho_simulation::tycho_common::models::Address;
use tycho_simulation::tycho_common::Bytes;
use utoipa::OpenApi;

use crate::api::error::ErrorResponse;

/// OpenAPI documentation bundle for the stable Fynd RPC endpoints.
#[derive(OpenApi)]
#[openapi(
    paths(handlers::quote, handlers::health, handlers::info),
    components(schemas(
        dto::QuoteRequest,
        dto::Order,
        dto::OrderSide,
        dto::QuoteOptions,
        dto::PriceGuardConfig,
        dto::Quote,
        dto::OrderQuote,
        dto::QuoteStatus,
        dto::Route,
        dto::Swap,
        dto::BlockInfo,
        dto::InstanceInfo,
        HealthStatus,
        ErrorResponse,
    ))
)]
pub struct ApiDoc;

#[cfg(feature = "experimental")]
/// OpenAPI documentation bundle for experimental endpoints (`GET /v1/prices`,
/// `GET /v1/tokens`).
#[derive(OpenApi)]
#[openapi(
    paths(handlers::get_prices, handlers::get_tokens),
    components(schemas(
        prices::PricesResponse,
        prices::TokenPriceEntry,
        prices::SpotPriceEntry,
        prices::ComponentDepthEntry,
        tokens::TokensResponse,
        tokens::GraphTokenEntry,
    ))
)]
pub struct ExperimentalApiDoc;

/// Builds the OpenAPI contract for every endpoint compiled into this crate.
pub fn openapi_spec() -> utoipa::openapi::OpenApi {
    #[allow(unused_mut)]
    let mut openapi = ApiDoc::openapi();
    #[cfg(feature = "experimental")]
    {
        openapi.merge(ExperimentalApiDoc::openapi());
        // Mark experimental operations so spec consumers know the endpoint may not
        // exist on a non-experimental (default) build.
        for path in ["/v1/prices", "/v1/tokens"] {
            if let Some(operation) = openapi
                .paths
                .paths
                .get_mut(path)
                .and_then(|path_item| path_item.get.as_mut())
            {
                operation
                    .extensions
                    .get_or_insert_with(Default::default)
                    .insert("x-experimental".to_string(), serde_json::json!(true));
            }
        }
    }
    openapi
}

/// Simple tracker for service health metrics.
///
/// Reads the last update timestamp from MarketState to determine how fresh the market data is,
/// and checks derived data overall readiness.
#[derive(Clone)]
pub(crate) struct HealthTracker {
    market_data: MarketData,
    derived_data: SharedDerivedDataRef,
    gas_price_stale_threshold: Option<Duration>,
    created_at: Instant,
}

impl HealthTracker {
    /// Creates a new health tracker.
    pub(crate) fn new(market_data: MarketData, derived_data: SharedDerivedDataRef) -> Self {
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
            .derived_data_ready()
    }
}

/// Shared application state for HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    worker_router: Arc<WorkerPoolRouter>,
    health_tracker: HealthTracker,
    chain_id: u64,
    router_address: Option<Bytes>,
    permit2_address: Bytes,
    #[cfg(feature = "experimental")]
    pub(crate) derived_data: SharedDerivedDataRef,
    #[cfg(feature = "experimental")]
    pub(crate) gas_token: Address,
    #[cfg(feature = "experimental")]
    pub(crate) market_data: MarketData,
    #[cfg(feature = "experimental")]
    pub(crate) tokens_cache: Arc<tokio::sync::RwLock<Option<tokens::TokensCache>>>,
}

impl AppState {
    /// Creates new application state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        worker_router: WorkerPoolRouter,
        health_tracker: HealthTracker,
        chain_id: u64,
        router_address: Option<Bytes>,
        permit2_address: Bytes,
        #[cfg(feature = "experimental")] derived_data: SharedDerivedDataRef,
        #[cfg(feature = "experimental")] gas_token: Address,
        #[cfg(feature = "experimental")] market_data: MarketData,
    ) -> Self {
        Self {
            worker_router: Arc::new(worker_router),
            health_tracker,
            chain_id,
            router_address,
            permit2_address,
            #[cfg(feature = "experimental")]
            derived_data,
            #[cfg(feature = "experimental")]
            gas_token,
            #[cfg(feature = "experimental")]
            market_data,
            #[cfg(feature = "experimental")]
            tokens_cache: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    pub(crate) fn worker_router(&self) -> &Arc<WorkerPoolRouter> {
        &self.worker_router
    }

    pub(crate) fn health_tracker(&self) -> &HealthTracker {
        &self.health_tracker
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub(crate) fn router_address(&self) -> Option<&Bytes> {
        self.router_address.as_ref()
    }

    pub(crate) fn permit2_address(&self) -> &Bytes {
        &self.permit2_address
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
///
/// `hosted_swagger_url` names the hosted gateway the `/docs/hosted/` UI points at; when it is
/// `None` that UI is not served.
pub(crate) fn configure_app(
    cfg: &mut web::ServiceConfig,
    state: AppState,
    hosted_swagger_url: Option<String>,
) {
    cfg.configure(configure_error_handlers)
        .app_data(web::Data::new(state))
        .configure(configure_routes)
        .configure(|cfg| docs::configure_docs(cfg, hosted_swagger_url.as_deref()))
        .default_service(web::to(|| async {
            let body = ErrorResponse::new("not found".into(), "NOT_FOUND".into());
            HttpResponse::NotFound().json(body)
        }));
}

#[cfg(all(test, feature = "experimental"))]
mod openapi_tests {
    #[test]
    fn test_openapi_spec_marks_prices_experimental() {
        let spec = serde_json::to_value(super::openapi_spec()).unwrap();

        assert!(spec["paths"]["/v1/prices"].is_object());
        let price = &spec["components"]["schemas"]["TokenPriceEntry"]["properties"]["price"];
        assert_eq!(price["type"], "string", "price must serialize as a decimal string");
        assert_eq!(price["example"], "0.000000003");

        // Experimental operations must be marked so spec consumers know they may not exist
        // on a non-experimental build.
        for path in ["/v1/prices", "/v1/tokens"] {
            assert!(spec["paths"][path].is_object());
            let ext = &spec["paths"][path]["get"]["x-experimental"];
            assert_eq!(ext, true, "x-experimental extension must be true on {path}");
        }
    }
}
