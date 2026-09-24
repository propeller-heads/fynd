//! HTTP request handlers for the solver API.

#[cfg(feature = "experimental")]
use std::{sync::Arc, time::Instant};

use actix_web::{web, HttpRequest, HttpResponse};
use tracing::instrument;
#[cfg(feature = "experimental")]
use tracing::{debug, info, warn};

use super::{dto, ApiError, AppState, RouteConfigurator};
#[cfg(feature = "experimental")]
use crate::api::prices::{
    price_to_decimal_string, ComponentDepthEntry, ComputationDataStatus, ComputationDataStatuses,
    DataStatus, IncludeField, PricesQuery, PricesResponse, SpotPriceEntry, TokenPriceEntry,
    TychoDataStatus,
};
#[cfg(feature = "experimental")]
use crate::api::tokens::{build_token_entries, TokensCache, TokensQuery, TokensResponse};
use crate::api::{
    disable_slippage_taking,
    error::ErrorResponse,
    exclusive_access,
    middleware::ClientInfo,
    record::{self, QuoteRecord, RequestRecord},
    request_capture::{log_request_capture, log_slow_solve, SLOW_SOLVE_THRESHOLD_MS},
};

/// Configures API routes under the `/v1` namespace.
///
/// `overrides`, when set, adds its routes to the scope before the defaults below, so a route it
/// registers for the same path and method wins; see [`RouteConfigurator`].
pub(crate) fn configure_routes(
    cfg: &mut web::ServiceConfig,
    state: &AppState,
    overrides: Option<&RouteConfigurator>,
) {
    let mut scope = web::scope("/v1");
    if let Some(overrides) = overrides {
        scope = overrides(scope, state);
    }
    let scope = scope
        .route("/quote", web::post().to(quote))
        .route("/health", web::get().to(health))
        .route("/info", web::get().to(info));
    #[cfg(feature = "experimental")]
    let scope = scope
        .route("/prices", web::get().to(get_prices))
        .route("/tokens", web::get().to(get_tokens));
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
/// - 503 Service Unavailable: Queue full, service overloaded, or quote timeout
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
        (status = 503, description = "Queue full, overloaded, stale data, or timeout", body = ErrorResponse),
    )
)]
#[instrument(skip(state, request, http_request), fields(num_orders = request.orders().len()))]
pub async fn quote(
    state: web::Data<AppState>,
    request: web::Json<dto::QuoteRequest>,
    http_request: HttpRequest,
) -> Result<HttpResponse, ApiError> {
    let access = exclusive_access::from_headers(http_request.headers());
    let core_request = validate_quote_request(request.into_inner())?;
    let core_request = disable_slippage_taking::apply(
        core_request,
        disable_slippage_taking::from_headers(http_request.headers()),
    );
    let request_record = RequestRecord::capture(&core_request, access);

    let result = state
        .worker_router()
        .quote(core_request, access)
        .await;
    // Reading the head takes the market-data lock, so only a quote that priced nothing asks.
    let head = match result
        .as_ref()
        .ok()
        .and_then(record::priced_block)
    {
        Some(_) => None,
        None => state
            .health_tracker()
            .tycho_head_status()
            .await
            .map(|status| status.head),
    };
    let client = ClientInfo::from_request(&http_request);
    let record = QuoteRecord::build(request_record, &result, state.chain(), client, head);
    log_quote_outcome(&record);
    if let Some(emitter) = state.record_emitter() {
        emitter.emit(record);
    }

    let dto_quote: dto::Quote = result?.into();
    Ok(HttpResponse::Ok().json(dto_quote))
}

/// Validates a wire-format quote request and converts it to the core type.
///
/// Rejects requests without orders and orders that fail [`fynd_core::Order::validate`]. Take a
/// [`RequestRecord::capture`] of the returned request before solving it, so the outcome can be
/// recorded with [`QuoteRecord::build`] and logged with [`log_quote_outcome`].
pub fn validate_quote_request(
    request: dto::QuoteRequest,
) -> Result<fynd_core::QuoteRequest, ApiError> {
    if request.orders().is_empty() {
        return Err(ApiError::BadRequest("no orders provided".to_string()));
    }
    let core_request: fynd_core::QuoteRequest = request.into();
    for order in core_request.orders() {
        if let Err(e) = order.validate() {
            return Err(ApiError::BadRequest(format!("invalid order {}: {}", order.id(), e)));
        }
    }
    Ok(core_request)
}

/// Emits the failure-capture and slow-solve log lines for a finished quote.
///
/// Both lines read the request and the outcome off `record`, so they cannot disagree with what
/// the collector receives. Serialization happens on a detached task carrying the current span,
/// so it never adds latency to the response. Successful, fast quotes log nothing.
///
/// # Panics
///
/// Spawns the detached task with [`actix_web::rt::spawn`], which panics when called outside a
/// running Actix system. Callers must invoke this from an Actix worker (i.e. inside a handler).
pub fn log_quote_outcome(record: &QuoteRecord) {
    let is_failure = record.is_failure();
    let slow_solve_time_ms = record.slow_solve_time_ms();
    if !is_failure && slow_solve_time_ms.is_none() {
        return;
    }
    // Past the guard: this quote logs, so the copies below are not wasted.
    let capture = record.replay().clone();
    let num_orders = capture.num_orders();
    let outcome = record.log_outcome();
    let span = tracing::Span::current();
    actix_web::rt::spawn(async move {
        span.in_scope(|| {
            let replay_json = capture.to_json();
            if is_failure {
                log_request_capture(num_orders, &replay_json, &outcome);
            }
            if let Some(solve_time_ms) = slow_solve_time_ms {
                log_slow_solve(solve_time_ms, num_orders, SLOW_SOLVE_THRESHOLD_MS, &replay_json);
            }
        });
    });
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
    let age_ms = state.health_tracker().age_ms().await;
    let data_fresh = age_ms < 60_000; // Healthy if data less than 60s old
    let derived_data_ready = state
        .health_tracker()
        .derived_data_ready()
        .await;
    let gas_price_age_ms = state
        .health_tracker()
        .gas_price_age_ms()
        .await;
    let gas_stale = state
        .health_tracker()
        .gas_price_stale()
        .await;
    let is_healthy = data_fresh && derived_data_ready && !gas_stale;

    let status = dto::HealthStatus::new(
        is_healthy,
        age_ms,
        state.worker_router().num_pools(),
        derived_data_ready,
        gas_price_age_ms,
    );

    if is_healthy {
        HttpResponse::Ok().json(status)
    } else {
        HttpResponse::ServiceUnavailable().json(status)
    }
}

/// GET /v1/info - Return static metadata about this Fynd instance.
#[utoipa::path(
    get,
    path = "/v1/info",
    tag = "solver",
    responses(
        (status = 200, description = "Instance info", body = dto::InstanceInfo),
    )
)]
pub async fn info(state: web::Data<AppState>) -> HttpResponse {
    let body = dto::InstanceInfo::builder(
        state.chain_id(),
        state
            .router_address()
            .cloned()
            .map(Into::into),
        state.permit2_address().clone().into(),
    )
    .version(env!("CARGO_PKG_VERSION"))
    .build();
    HttpResponse::Ok().json(body)
}

#[cfg(feature = "experimental")]
/// Default limit for spot_prices and component_depths entries.
const DEFAULT_PRICES_LIMIT: usize = 1000;

#[cfg(feature = "experimental")]
/// Maximum accepted limit for spot_prices and component_depths entries.
const MAX_PRICES_LIMIT: usize = 1000;

#[cfg(feature = "experimental")]
struct PricesDataSnapshot {
    token_prices: Arc<fynd_core::derived::types::TokenGasPrices>,
    spot_prices: Option<fynd_core::derived::types::SpotPrices>,
    component_depths: Option<fynd_core::derived::types::ComponentDepths>,
    computations: ComputationDataStatuses,
}

#[cfg(feature = "experimental")]
/// GET /v1/prices - Return per-token mid prices and optional market data.
///
/// Returns 503 with `NOT_READY` until the first token-price solve has landed and a Tycho head is
/// available. In the production feed lifecycle, the Tycho head lands before derived computations.
/// Each `prices[].price` is a plain decimal string holding raw target-token units divided by raw
/// gas-token units; consumers must normalize both tokens' decimals before using it. Use the
/// `include` query parameter to add spot prices and/or component depths.
///
/// # Query Parameters
///
/// - `include` - Comma-separated list: `depths`, `spot_prices`
/// - `limit` - Max entries for spot_prices / component_depths (default and maximum: 1000)
#[utoipa::path(
    get,
    path = "/v1/prices",
    tag = "prices",
    params(PricesQuery),
    responses(
        (status = 200, description = "Prices returned", body = PricesResponse),
        (status = 400, description = "Invalid query parameter or limit exceeds 1000", body = ErrorResponse),
        (status = 503, description = "NOT_READY: token prices, requested computations, or Tycho head are not yet available", body = ErrorResponse),
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
    if limit > MAX_PRICES_LIMIT {
        return Err(ApiError::BadRequest(format!("limit must be at most {MAX_PRICES_LIMIT}")));
    }
    let want_depths = include_fields.contains(&IncludeField::Depths);
    let want_spot = include_fields.contains(&IncludeField::SpotPrices);

    let snapshot = {
        let store = state.derived_data.read().await;
        let now = Instant::now();
        let as_data_status = |status: fynd_core::derived::ComputationStatus| {
            ComputationDataStatus { block: status.block(), last_update_ms: status.age_ms_at(now) }
        };
        let (token_prices, token_prices_status) = store
            .token_prices_with_status()
            .ok_or_else(|| ApiError::NotReady("Token prices have not been computed".to_string()))?;
        let spot_prices = store.spot_prices_with_status();
        let spot_prices_data = match (want_spot, spot_prices) {
            (true, Some((prices, _))) => Some(prices.clone()),
            (true, None) => {
                return Err(ApiError::NotReady("Spot prices have not been computed".to_string()));
            }
            (false, _) => None,
        };
        let component_depths = store.component_depths_with_status();
        let component_depths_data = match (want_depths, component_depths) {
            (true, Some((depths, _))) => Some(depths.clone()),
            (true, None) => {
                return Err(ApiError::NotReady(
                    "Component depths have not been computed".to_string(),
                ));
            }
            (false, _) => None,
        };

        PricesDataSnapshot {
            token_prices: Arc::clone(token_prices),
            spot_prices: spot_prices_data,
            component_depths: component_depths_data,
            computations: ComputationDataStatuses {
                token_prices: as_data_status(token_prices_status),
                spot_prices: spot_prices.map(|(_, status)| as_data_status(status)),
                component_depths: component_depths.map(|(_, status)| as_data_status(status)),
            },
        }
    };

    // Derived data is checked first to preserve error precedence. Production computations only run
    // after a Tycho head is available, so a missing head with computed data is fixture-only state.
    let tycho_status = state
        .health_tracker()
        .tycho_head_status()
        .await
        .ok_or_else(|| ApiError::NotReady("Tycho head is unavailable".to_string()))?;

    let mut prices = Vec::new();
    let mut skipped_tokens = 0usize;
    for (address, price) in snapshot.token_prices.iter() {
        match price_to_decimal_string(&price.numerator, &price.denominator) {
            Some(price) => prices.push(TokenPriceEntry { token: address.clone(), price }),
            None => {
                debug!(
                    token = %address,
                    "cannot serialize token price (zero or oversized numerator/denominator)"
                );
                skipped_tokens += 1;
            }
        }
    }
    if skipped_tokens > 0 {
        warn!(
            skipped_tokens,
            "skipped tokens with unrepresentable prices (zero or oversized numerator/denominator)"
        );
    }
    // Sort for a deterministic wire order; HashMap iteration order varies per process.
    prices.sort_by(|a, b| a.token.cmp(&b.token));
    // Convert spot prices if requested (sorted for deterministic limit)
    let spot_prices = if want_spot {
        let mut entries: Vec<SpotPriceEntry> = snapshot
            .spot_prices
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

    // Convert component depths if requested (sorted for deterministic limit)
    let component_depths = if want_depths {
        let mut entries: Vec<ComponentDepthEntry> = snapshot
            .component_depths
            .into_iter()
            .flatten()
            .map(|((component_id, token_in, token_out), depth)| ComponentDepthEntry {
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
        data_status: DataStatus {
            tycho: TychoDataStatus {
                head: tycho_status.head.into(),
                last_update_ms: tycho_status.last_update_ms,
            },
            computations: snapshot.computations,
        },
        spot_prices,
        component_depths,
    };

    info!(
        num_tokens = response.prices.len(),
        has_spot = response.spot_prices.is_some(),
        has_depths = response.component_depths.is_some(),
        "prices response"
    );

    Ok(HttpResponse::Ok().json(response))
}

#[cfg(feature = "experimental")]
/// Default maximum number of tokens returned by GET /v1/tokens.
const DEFAULT_TOKENS_LIMIT: usize = 1000;

#[cfg(feature = "experimental")]
/// Maximum accepted limit for GET /v1/tokens.
const MAX_TOKENS_LIMIT: usize = 1000;

#[cfg(feature = "experimental")]
/// GET /v1/tokens - Return the tokens currently in the routing graph, ranked by usefulness.
///
/// Serves metadata (symbol, decimals, tax, gas, quality) for exactly the tokens present in
/// the routing graph, sorted by approximate routable `liquidity` in raw gas-token units
/// (descending), then `component_count`, then address. The list is recomputed lazily at
/// most once per derived-data update and cached; nothing runs on the quote path.
///
/// Paginate with `offset`/`limit` (e.g. `?limit=100&offset=1000` returns tokens ranked
/// #1001-#1100). Pages are consistent while the response `block` is unchanged; restart
/// from offset 0 when it advances mid-pagination.
///
/// # Query Parameters
///
/// - `limit` - Maximum number of tokens returned (default and maximum: 1000)
/// - `offset` - Number of tokens to skip from the start of the ranked list (default: 0)
#[utoipa::path(
    get,
    path = "/v1/tokens",
    tag = "tokens",
    params(TokensQuery),
    responses(
        (status = 200, description = "Graph tokens returned", body = TokensResponse),
        (status = 400, description = "Invalid query parameter or limit exceeds 1000", body = ErrorResponse),
        (status = 503, description = "NOT_READY: token prices have not yet been computed", body = ErrorResponse),
    )
)]
#[instrument(skip(state))]
pub async fn get_tokens(
    state: web::Data<AppState>,
    query: web::Query<TokensQuery>,
) -> Result<HttpResponse, ApiError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TOKENS_LIMIT);
    if limit > MAX_TOKENS_LIMIT {
        return Err(ApiError::BadRequest(format!("limit must be at most {MAX_TOKENS_LIMIT}")));
    }
    let offset = query.offset.unwrap_or(0);

    let cache_key = {
        let store = state.derived_data.read().await;
        let token_prices_block = store
            .token_prices_with_status()
            .map(|(_, status)| status.block())
            .ok_or_else(|| ApiError::NotReady("Token prices have not been computed".to_string()))?;
        (token_prices_block, store.component_depths_block())
    };

    if let Some(cache) = state.tokens_cache.read().await.as_ref() {
        if cache.key == cache_key {
            return Ok(tokens_response(cache, limit, offset));
        }
    }

    // Re-derive the key together with the data so the cache entry matches what it holds,
    // even if a computation lands between the check above and this clone.
    let (key, token_prices, depths) = {
        let store = state.derived_data.read().await;
        let (token_prices, token_prices_status) = store
            .token_prices_with_status()
            .ok_or_else(|| ApiError::NotReady("Token prices have not been computed".to_string()))?;
        let component_depths = store.component_depths_with_status();
        (
            (token_prices_status.block(), component_depths.map(|(_, status)| status.block())),
            Arc::clone(token_prices),
            component_depths.map(|(depths, _)| depths.clone()),
        )
    };

    // Snapshot under the read guard and rank outside it, so the per-block feed writer
    // is never blocked by the fold over the full topology.
    let (topology, token_registry) = {
        let market = state.market_data.read().await;
        (market.component_topology(), market.token_registry_ref().clone())
    };
    let entries = build_token_entries(
        &topology,
        &token_registry,
        depths.as_ref(),
        Some(token_prices.as_ref()),
    );

    let cache = TokensCache { key, entries: std::sync::Arc::new(entries) };
    let response = tokens_response(&cache, limit, offset);
    info!(num_tokens = cache.entries.len(), block = key.0, "tokens list recomputed");
    *state.tokens_cache.write().await = Some(cache);

    Ok(response)
}

#[cfg(feature = "experimental")]
/// Serializes one page of a cached token list: `offset` skips into the ranked
/// list, `limit` sizes the page. An offset past the end yields an empty page.
fn tokens_response(cache: &TokensCache, limit: usize, offset: usize) -> HttpResponse {
    let tokens: Vec<_> = cache
        .entries
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();
    HttpResponse::Ok().json(TokensResponse {
        total: cache.entries.len(),
        block: cache.key.0,
        tokens,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "experimental")]
    use std::str::FromStr;
    use std::sync::Arc;

    use actix_web::{test, web, App, HttpResponse};
    use fynd_core::{
        derived::SharedDerivedDataRef,
        encoding::encoder::Encoder,
        feed::market_data::MarketData,
        worker_pool_router::{config::WorkerPoolRouterConfig, WorkerPoolRouter},
    };
    use serde_json::Value;
    use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
    use tycho_simulation::tycho_common::{models::Chain, Bytes};
    #[cfg(feature = "experimental")]
    use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

    #[cfg(feature = "experimental")]
    use crate::api::tokens::{GraphTokenEntry, TokensCache};
    use crate::api::{dto::QuoteRequest, record_emitter::RecordEmitter, AppState, HealthTracker};

    // Nested so it doesn't inherit this module's unqualified `test` import above (actix-web
    // exports both a `test` module and a `#[test]` attribute macro at that path; the import
    // shadows the standard library's `#[test]`, which then rejects these non-async fns). See
    // the identical note in `docs.rs`.
    mod quote_pipeline {

        use crate::api::{dto, handlers::validate_quote_request, ApiError};

        fn dto_request(orders: Vec<dto::Order>) -> dto::QuoteRequest {
            serde_json::from_value(serde_json::json!({
                "orders": orders,
                "options": {}
            }))
            .expect("valid request json")
        }

        fn dto_order() -> dto::Order {
            serde_json::from_value(serde_json::json!({
                "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                "amount": "1000000000000000000",
                "side": "sell",
                "sender": "0x000000000000000000000000000000000000dEaD"
            }))
            .expect("valid order json")
        }

        #[test]
        fn test_validate_quote_request_rejects_empty_orders() {
            let err = validate_quote_request(dto_request(vec![])).expect_err("empty orders");
            assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
        }

        #[test]
        fn test_validate_quote_request_converts_valid_orders() {
            let core_request =
                validate_quote_request(dto_request(vec![dto_order()])).expect("valid");
            assert_eq!(core_request.orders().len(), 1);
        }
    }

    /// Minimal handler that mirrors the real quote handler's JSON extraction.
    /// The body deserialization error happens before this is called.
    async fn echo_quote(_req: web::Json<QuoteRequest>) -> HttpResponse {
        HttpResponse::Ok().finish()
    }

    /// Creates a test service that mirrors `configure_app`'s extractor setup.
    /// This intentionally matches the real server's `configure_app` call so that
    /// fixes to the app config are reflected here.
    macro_rules! make_test_app {
        () => {
            test::init_service(
                App::new()
                    .configure(crate::api::configure_error_handlers)
                    .route("/v1/quote", web::post().to(echo_quote)),
            )
            .await
        };
    }

    async fn body_json(resp: actix_web::dev::ServiceResponse) -> Value {
        let bytes = test::read_body(resp).await;
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn make_test_state() -> AppState {
        make_test_state_with_emitter(None)
    }

    fn make_test_state_with_emitter(record_emitter: Option<RecordEmitter>) -> AppState {
        let market_data: MarketData = MarketData::new_shared();
        let derived_data: SharedDerivedDataRef =
            Arc::new(tokio::sync::RwLock::new(Default::default()));

        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(None)
            .expect("default encoders should always succeed");
        let encoder = Encoder::new(Chain::Ethereum, registry).expect("encoder should build");

        let router = WorkerPoolRouter::new(vec![], WorkerPoolRouterConfig::default(), encoder);
        let health_tracker = HealthTracker::new(market_data.clone(), Arc::clone(&derived_data));

        let router_address =
            Bytes::from(hex::decode("fD0b31d2E955fA55e3fa641Fe90e08b677188d35").unwrap());
        let permit2_address =
            Bytes::from(hex::decode("000000000022D473030F116dDEE9F6B43aC78BA3").unwrap());

        AppState::new(
            router,
            health_tracker,
            Chain::Ethereum,
            Some(router_address),
            permit2_address,
            record_emitter,
            #[cfg(feature = "experimental")]
            derived_data,
            #[cfg(feature = "experimental")]
            tycho_simulation::tycho_common::models::Address::from([0u8; 20]),
            #[cfg(feature = "experimental")]
            market_data,
        )
    }

    /// Answers `requests` quotes against `state` and returns the status code of each.
    async fn quote_statuses(state: AppState, requests: usize) -> Vec<u16> {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/quote", web::post().to(super::quote)),
        )
        .await;
        let mut statuses = Vec::with_capacity(requests);
        for _ in 0..requests {
            let request = test::TestRequest::post()
                .uri("/v1/quote")
                .set_json(serde_json::json!({
                    "orders": [{
                        "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                        "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                        "amount": "1000000000000000000",
                        "side": "sell",
                        "sender": "0x000000000000000000000000000000000000dEaD"
                    }]
                }))
                .to_request();
            statuses.push(
                test::call_service(&app, request)
                    .await
                    .status()
                    .as_u16(),
            );
        }
        statuses
    }

    /// A record queue with no room changes neither the answer nor how long it takes: the first
    /// quote fills the one slot, the second finds it full and has its record dropped. The
    /// timeout is what catches a queue that makes the handler wait — a blocking send would never
    /// return.
    #[actix_web::test]
    async fn test_quote_answers_with_a_full_record_queue() {
        let (emitter, mut receiver) =
            RecordEmitter::new(std::num::NonZeroUsize::new(1).expect("one is not zero"));
        let without_queue = quote_statuses(make_test_state(), 1).await;

        let with_full_queue = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            quote_statuses(make_test_state_with_emitter(Some(emitter)), 2),
        )
        .await
        .expect("both quotes answered");

        assert_eq!(with_full_queue, vec![without_queue[0]; 2]);
        assert!(receiver.try_recv().is_ok(), "the first quote's record was queued");
        assert!(receiver.try_recv().is_err(), "the second quote's record was dropped");
    }

    #[cfg(feature = "experimental")]
    async fn seed_tycho_head(state: &AppState) {
        state
            .market_data
            .write()
            .await
            .update_last_updated(fynd_core::BlockInfo::new(
                19_000_001,
                "0xprices-head".to_string(),
                1_700_000_000,
            ));
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_rejects_limit_over_maximum_before_derived_data() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/prices?limit=1001")
                .to_request(),
        )
        .await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_not_ready() {
        for (seed_head, seed_token_prices, uri, expected_error) in [
            (false, true, "/v1/prices", "data not ready: Tycho head is unavailable"),
            (false, false, "/v1/prices", "data not ready: Token prices have not been computed"),
            (true, false, "/v1/prices", "data not ready: Token prices have not been computed"),
            (
                true,
                true,
                "/v1/prices?include=spot_prices",
                "data not ready: Spot prices have not been computed",
            ),
            (
                true,
                true,
                "/v1/prices?include=depths",
                "data not ready: Component depths have not been computed",
            ),
        ] {
            let state = make_test_state();
            if seed_head {
                seed_tycho_head(&state).await;
            }
            if seed_token_prices {
                state
                    .derived_data
                    .write()
                    .await
                    .set_token_prices(Default::default(), vec![], 19_000_000, true);
            }
            let app = test::init_service(
                App::new()
                    .app_data(web::Data::new(state))
                    .route("/v1/prices", web::get().to(super::get_prices)),
            )
            .await;

            let resp = test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(uri)
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status().as_u16(), 503, "uri: {uri}");
            let body = body_json(resp).await;
            assert_eq!(body["code"], "NOT_READY", "uri: {uri}; body was: {body}");
            assert_eq!(body["error"], expected_error, "uri: {uri}; body was: {body}");
        }
    }

    // The pricing pass cannot fail as a whole, so its first run sets the block even when
    // nothing but the gas token (priced 1:1 unconditionally) is in the map. That is a landed
    // answer: on a market where no pool trades the gas token, the one-entry map is the honest
    // response, and a 503 would never clear.
    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_with_only_the_gas_token_priced() {
        use num_bigint::BigUint;
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        let state = make_test_state();
        seed_tycho_head(&state).await;
        {
            let mut store = state.derived_data.write().await;
            store.set_token_prices(
                [(test_addr(0x00), Price::new(BigUint::from(1u8), BigUint::from(1u8)))]
                    .into_iter()
                    .collect(),
                vec![],
                19_000_000,
                true,
            );
        }
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/prices")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["prices"].as_array().map(Vec::len), Some(1), "body was: {body}");
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_returns_200_once_a_token_is_priced() {
        use num_bigint::BigUint;
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        let state = make_test_state();
        seed_tycho_head(&state).await;
        {
            let mut store = state.derived_data.write().await;
            store.set_token_prices(
                [
                    (test_addr(0x00), Price::new(BigUint::from(1u8), BigUint::from(1u8))),
                    (test_addr(0x0b), Price::new(BigUint::from(2u8), BigUint::from(1u8))),
                ]
                .into_iter()
                .collect(),
                vec![],
                19_000_000,
                true,
            );
        }
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/prices")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["prices"].as_array().map(Vec::len), Some(2), "body was: {body}");
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_data_status() {
        const MAX_TEST_COMPUTATION_AGE_MS: u64 = 60_000;

        let state = make_test_state();
        seed_tycho_head(&state).await;
        {
            let mut store = state.derived_data.write().await;
            store.set_token_prices(Default::default(), vec![], 19_000_000, true);
            store.set_spot_prices(Default::default(), vec![], 18_999_999, true);
            store.set_component_depths(Default::default(), vec![], 18_999_998, true);
        }
        let state = web::Data::new(state);
        let app = test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;

        let tycho_age_before = state.health_tracker().age_ms().await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/prices")
                .to_request(),
        )
        .await;
        let tycho_age_after = state.health_tracker().age_ms().await;
        assert_eq!(response.status().as_u16(), 200);
        let body = body_json(response).await;

        assert!(body["data_status"].is_object(), "body was: {body}");
        assert!(body.get("blocks").is_none(), "body was: {body}");
        assert!(body.get("spot_prices").is_none(), "body was: {body}");
        assert!(body.get("component_depths").is_none(), "body was: {body}");

        let tycho_status = &body["data_status"]["tycho"];
        assert_eq!(tycho_status["head"]["number"], 19_000_001);
        assert_eq!(tycho_status["head"]["hash"], "0xprices-head");
        assert_eq!(tycho_status["head"]["timestamp"], 1_700_000_000);
        let tycho_age = tycho_status["last_update_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("Tycho age must be a u64: {body}"));
        assert!(
            (tycho_age_before..=tycho_age_after).contains(&tycho_age),
            "Tycho age {tycho_age}ms was outside request bounds \
             {tycho_age_before}..={tycho_age_after}: {body}"
        );

        let computations = &body["data_status"]["computations"];
        for (status_name, expected_block) in [
            ("token_prices", 19_000_000),
            ("spot_prices", 18_999_999),
            ("component_depths", 18_999_998),
        ] {
            let status = &computations[status_name];
            assert_eq!(status["block"], expected_block, "body was: {body}");
            let age_ms = status["last_update_ms"]
                .as_u64()
                .unwrap_or_else(|| panic!("{status_name} age must be a u64: {body}"));
            assert!(
                age_ms <= MAX_TEST_COMPUTATION_AGE_MS,
                "{status_name} age {age_ms}ms was unexpectedly high: {body}"
            );
        }
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_applies_limit_boundaries() {
        use num_bigint::BigUint;

        let state = make_test_state();
        seed_tycho_head(&state).await;
        let token = test_addr(1);
        let token_in = test_addr(2);
        let token_out = test_addr(3);
        let spot_prices = (0usize..1001)
            .map(|index| {
                (
                    (format!("component-{index:04}"), token_in.clone(), token_out.clone()),
                    index as f64,
                )
            })
            .collect();
        let component_depths = (0usize..1001)
            .map(|index| {
                (
                    (format!("component-{index:04}"), token_in.clone(), token_out.clone()),
                    BigUint::from(index + 1),
                )
            })
            .collect();
        {
            let mut store = state.derived_data.write().await;
            store.set_token_prices(
                [(token, Price::new(1u8.into(), 1u8.into()))]
                    .into_iter()
                    .collect(),
                vec![],
                19_000_000,
                true,
            );
            store.set_spot_prices(spot_prices, vec![], 19_000_000, true);
            store.set_component_depths(component_depths, vec![], 19_000_000, true);
        }

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;

        for uri in [
            "/v1/prices?include=spot_prices,depths",
            "/v1/prices?include=spot_prices,depths&limit=1000",
        ] {
            let body: Value = test::call_and_read_body_json(
                &app,
                test::TestRequest::get()
                    .uri(uri)
                    .to_request(),
            )
            .await;
            assert_eq!(body["prices"].as_array().unwrap().len(), 1, "body was: {body}");
            assert_eq!(
                body["spot_prices"]
                    .as_array()
                    .unwrap()
                    .len(),
                1000,
                "body was: {body}"
            );
            assert_eq!(
                body["component_depths"]
                    .as_array()
                    .unwrap()
                    .len(),
                1000,
                "body was: {body}"
            );
        }

        let zero_limit: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/prices?include=spot_prices,depths&limit=0")
                .to_request(),
        )
        .await;
        assert_eq!(
            zero_limit["prices"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(zero_limit["spot_prices"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(zero_limit["component_depths"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_serializes_decimal_strings() {
        let gas_token = "0x0000000000000000000000000000000000000001";
        // (address, numerator, denominator, expected decimal string), pre-sorted by address
        // because the handler sorts entries for a deterministic wire order.
        let cases = [
            ("0x0000000000000000000000000000000000000006", 3u128, 1_000_000_000u128, "0.000000003"),
            ("0x0000000000000000000000000000000000000008", 5, 1_000_000_000_000, "0.000000000005"),
            ("0x0000000000000000000000000000000000000018", 1500, 1, "1500"),
        ];
        let mut state = make_test_state();
        state.gas_token =
            tycho_simulation::tycho_common::models::Address::from_str(gas_token).unwrap();
        seed_tycho_head(&state).await;
        let mut token_prices = rustc_hash::FxHashMap::default();
        for (address, numerator, denominator) in
            cases.map(|(address, numerator, denominator, _)| (address, numerator, denominator))
        {
            token_prices.insert(
                tycho_simulation::tycho_common::models::Address::from_str(address).unwrap(),
                Price::new(numerator.into(), denominator.into()),
            );
        }
        state
            .derived_data
            .write()
            .await
            .set_token_prices(token_prices, vec![], 19_000_000, true);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;
        let request = test::TestRequest::get()
            .uri("/v1/prices")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), 200);
        let body = body_json(response).await;

        assert!(body["gas_token"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case(gas_token));
        let response_prices = body["prices"].as_array().unwrap();
        assert_eq!(response_prices.len(), cases.len());
        for (entry, (address, _, _, expected_price)) in response_prices.iter().zip(cases) {
            assert!(entry["token"]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case(address));
            assert_eq!(entry["price"].as_str().unwrap(), expected_price);
        }
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_skips_non_serializable_prices() {
        let state = make_test_state();
        seed_tycho_head(&state).await;
        let mut token_prices = rustc_hash::FxHashMap::default();
        let valid = tycho_simulation::tycho_common::models::Address::from([1u8; 20]);
        token_prices.insert(valid.clone(), Price::new(1u8.into(), 2u8.into()));
        // Struct literal because Price::new panics on a zero numerator — this state is
        // constructor-unreachable, and the skip path is exercised defensively.
        token_prices.insert(
            tycho_simulation::tycho_common::models::Address::from([2u8; 20]),
            Price { numerator: 0u8.into(), denominator: 1u8.into() },
        );
        token_prices.insert(
            tycho_simulation::tycho_common::models::Address::from([3u8; 20]),
            Price::new(num_bigint::BigUint::from(10u8).pow(400), 1u8.into()),
        );
        token_prices.insert(
            tycho_simulation::tycho_common::models::Address::from([4u8; 20]),
            Price::new(1u8.into(), num_bigint::BigUint::from(10u8).pow(400)),
        );
        state
            .derived_data
            .write()
            .await
            .set_token_prices(token_prices, vec![], 19_000_000, true);

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/prices", web::get().to(super::get_prices)),
        )
        .await;
        let body: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/prices")
                .to_request(),
        )
        .await;

        let prices = body["prices"].as_array().unwrap();
        assert_eq!(prices.len(), 1);
        assert!(prices[0]["token"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case(&valid.to_string()));
        assert_eq!(prices[0]["price"], "0.5");
    }

    #[cfg(feature = "experimental")]
    fn test_addr(byte: u8) -> tycho_simulation::tycho_common::models::Address {
        tycho_simulation::tycho_common::models::Address::from([byte; 20])
    }

    #[cfg(feature = "experimental")]
    fn test_token(
        byte: u8,
        symbol: &str,
        decimals: u32,
    ) -> tycho_simulation::tycho_common::models::token::Token {
        tycho_simulation::tycho_common::models::token::Token {
            address: test_addr(byte),
            symbol: symbol.to_string(),
            decimals,
            tax: 0,
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    #[cfg(feature = "experimental")]
    fn test_component(
        id: &str,
        token_bytes: &[u8],
    ) -> tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
        tycho_simulation::tycho_common::models::protocol::ProtocolComponent::new(
            id,
            "uniswap_v2",
            "swap",
            Chain::Ethereum,
            token_bytes
                .iter()
                .map(|byte| test_addr(*byte))
                .collect(),
            vec![],
            std::collections::HashMap::new(),
            tycho_simulation::tycho_common::models::ChangeType::Creation,
            Default::default(),
            Default::default(),
        )
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_tokens_handler_returns_ranked_graph_tokens() {
        use num_bigint::BigUint;
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        let addr = test_addr;
        let state = make_test_state();
        {
            let mut market = state.market_data.write().await;
            market.upsert_tokens([
                test_token(0x0a, "WETH", 18),
                test_token(0x0b, "USDC", 6),
                test_token(0x0c, "OBSCURE", 8),
            ]);
            market.upsert_components([
                test_component("c1", &[0x0a, 0x0b]),
                test_component("c2", &[0x0a, 0x0c]),
            ]);
        }
        {
            let mut store = state.derived_data.write().await;
            store.set_token_prices(
                [
                    (addr(0x0a), Price::new(BigUint::from(1u8), BigUint::from(1u8))),
                    (addr(0x0b), Price::new(BigUint::from(2u8), BigUint::from(1u8))),
                ]
                .into_iter()
                .collect(),
                vec![],
                19_000_000,
                true,
            );
            store.set_component_depths(
                [
                    (("c1".to_string(), addr(0x0a), addr(0x0b)), BigUint::from(100u32)),
                    (("c1".to_string(), addr(0x0b), addr(0x0a)), BigUint::from(400u32)),
                ]
                .into_iter()
                .collect(),
                vec![],
                19_000_000,
                true,
            );
        }

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/tokens", web::get().to(super::get_tokens)),
        )
        .await;
        let body: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens")
                .to_request(),
        )
        .await;

        assert_eq!(body["block"], 19_000_000);
        assert_eq!(body["total"], 3);
        let tokens = body["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 3);
        // USDC: 400 raw units deep at 2 USDC per gas unit = 200 gas units; WETH: 100 at 1 = 100.
        assert_eq!(tokens[0]["symbol"], "USDC");
        assert_eq!(tokens[0]["liquidity"], 200.0);
        assert_eq!(tokens[0]["component_count"], 1);
        assert_eq!(tokens[0]["decimals"], 6);
        assert_eq!(tokens[0]["quality"], 100);
        assert_eq!(tokens[1]["symbol"], "WETH");
        assert_eq!(tokens[1]["liquidity"], 100.0);
        assert_eq!(tokens[1]["component_count"], 2);
        // Unpriced token ranks last and omits the liquidity field.
        assert_eq!(tokens[2]["symbol"], "OBSCURE");
        assert!(tokens[2].get("liquidity").is_none());

        // Same derived state: a second request is served from the cache with a limit applied.
        let limited: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens?limit=1")
                .to_request(),
        )
        .await;
        assert_eq!(limited["total"], 3);
        assert_eq!(
            limited["tokens"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(limited["tokens"][0]["symbol"], "USDC");

        // A zero-sized page is valid and retains the total count.
        let zero_limit: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens?limit=0")
                .to_request(),
        )
        .await;
        assert_eq!(zero_limit["total"], 3);
        assert!(zero_limit["tokens"]
            .as_array()
            .unwrap()
            .is_empty());

        // Offset pages into the ranked list: limit=1&offset=1 is the #2 token.
        let paged: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens?limit=1&offset=1")
                .to_request(),
        )
        .await;
        assert_eq!(paged["total"], 3);
        assert_eq!(paged["tokens"][0]["symbol"], "WETH");

        // An offset past the end yields an empty page, not an error.
        let past_end: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens?offset=5")
                .to_request(),
        )
        .await;
        assert_eq!(past_end["total"], 3);
        assert!(past_end["tokens"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_tokens_handler_rejects_limit_over_maximum_before_derived_data() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/tokens", web::get().to(super::get_tokens)),
        )
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens?limit=1001")
                .to_request(),
        )
        .await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_tokens_handler_applies_limit_boundaries() {
        let state = make_test_state();
        state
            .derived_data
            .write()
            .await
            .set_token_prices(Default::default(), vec![], 19_000_000, true);
        let entries = (0..1001)
            .map(|index| {
                let mut address = [0u8; 20];
                address[12..].copy_from_slice(&(index as u64).to_be_bytes());
                GraphTokenEntry {
                    address: address.into(),
                    symbol: format!("TOKEN-{index}"),
                    decimals: 18,
                    tax: 0,
                    gas: vec![],
                    quality: 100,
                    component_count: 1,
                    liquidity: None,
                }
            })
            .collect();
        *state.tokens_cache.write().await =
            Some(TokensCache { key: (19_000_000, None), entries: Arc::new(entries) });

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/tokens", web::get().to(super::get_tokens)),
        )
        .await;

        for uri in ["/v1/tokens", "/v1/tokens?limit=1000"] {
            let body: Value = test::call_and_read_body_json(
                &app,
                test::TestRequest::get()
                    .uri(uri)
                    .to_request(),
            )
            .await;
            assert_eq!(body["total"], 1001, "body was: {body}");
            assert_eq!(body["tokens"].as_array().unwrap().len(), 1000, "body was: {body}");
        }
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_tokens_handler_returns_503_before_derived_data() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/tokens", web::get().to(super::get_tokens)),
        )
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/v1/tokens")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 503);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "NOT_READY", "body was: {body}");
        assert_eq!(
            body["error"], "data not ready: Token prices have not been computed",
            "body was: {body}"
        );
    }

    // ── Unknown route (default_service) ────────────────────────────────────

    #[actix_web::test]
    async fn test_unknown_route_returns_json_404() {
        use crate::api::error::ErrorResponse;

        let app = test::init_service(
            App::new()
                .configure(crate::api::configure_error_handlers)
                .route("/v1/quote", web::post().to(echo_quote))
                .default_service(web::to(|| async {
                    let body = ErrorResponse::new("not found".into(), "NOT_FOUND".into());
                    HttpResponse::NotFound().json(body)
                })),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/does-not-exist")
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 404);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "NOT_FOUND", "body was: {body}");
    }

    // ── JSON body errors ────────────────────────────────────────────────────

    #[actix_web::test]
    async fn test_malformed_json_returns_json_error() {
        let app = make_test_app!();
        let req = test::TestRequest::post()
            .uri("/v1/quote")
            .insert_header(("content-type", "application/json"))
            .set_payload("{not valid json}")
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
        assert!(body["error"].is_string(), "body was: {body}");
    }

    #[actix_web::test]
    async fn test_empty_body_returns_json_error() {
        let app = make_test_app!();
        let req = test::TestRequest::post()
            .uri("/v1/quote")
            .insert_header(("content-type", "application/json"))
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
        assert!(body["error"].is_string(), "body was: {body}");
    }

    #[actix_web::test]
    async fn test_wrong_content_type_returns_json_error() {
        let app = make_test_app!();
        let req = test::TestRequest::post()
            .uri("/v1/quote")
            .insert_header(("content-type", "text/plain"))
            .set_payload(r#"{"orders":[]}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
        assert!(body["error"].is_string(), "body was: {body}");
    }

    // ── Query-string errors (QueryConfig) ──────────────────────────────────
    //
    // The prices endpoint uses `web::Query<PricesQuery>` to extract URL query
    // params like `?limit=100&include=depths`. This is completely separate from
    // the JSON body: `JsonConfig` only applies to `web::Json<T>` (request body),
    // while `QueryConfig` applies to `web::Query<T>` (URL query string).
    //
    // Without `QueryConfig`, a request like `?limit=not-a-number` would trigger
    // actix-web's default `QueryPayloadError` handler which returns plain text.

    #[actix_web::test]
    async fn test_invalid_query_param_returns_json_error() {
        #[derive(serde::Deserialize)]
        struct Params {
            #[allow(dead_code)]
            limit: usize,
        }

        async fn handler(_: web::Query<Params>) -> HttpResponse {
            HttpResponse::Ok().finish()
        }

        let app = test::init_service(
            App::new()
                .configure(crate::api::configure_error_handlers)
                .route("/v1/prices", web::get().to(handler)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/prices?limit=not-a-number")
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
        assert!(body["error"].is_string(), "body was: {body}");
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_limit_deserialization_errors_return_json() {
        let app = test::init_service(
            App::new()
                .configure(crate::api::configure_error_handlers)
                .app_data(web::Data::new(make_test_state()))
                .route("/v1/prices", web::get().to(super::get_prices))
                .route("/v1/tokens", web::get().to(super::get_tokens)),
        )
        .await;

        for endpoint in ["/v1/prices", "/v1/tokens"] {
            for limit in ["not-a-number", "-1", "18446744073709551616"] {
                let resp = test::call_service(
                    &app,
                    test::TestRequest::get()
                        .uri(&format!("{endpoint}?limit={limit}"))
                        .to_request(),
                )
                .await;

                assert_eq!(resp.status().as_u16(), 400);
                let body = body_json(resp).await;
                assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
                assert!(body["error"].is_string(), "body was: {body}");
            }
        }
    }

    #[actix_web::test]
    async fn test_invalid_field_type_returns_json_error() {
        let app = make_test_app!();
        // `orders` must be an array, not a string
        let req = test::TestRequest::post()
            .uri("/v1/quote")
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"orders": "not-an-array"}"#)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "BAD_REQUEST", "body was: {body}");
        assert!(body["error"].is_string(), "body was: {body}");
    }

    // ── /v1/info endpoint ──────────────────────────────────────────────────

    #[actix_web::test]
    async fn test_info_returns_200_with_chain_id() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/info", web::get().to(super::info)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/info")
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn test_info_response_has_required_fields() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/info", web::get().to(super::info)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/info")
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

        assert_eq!(body["chain_id"], 1);
        assert!(body["router_address"].is_string(), "router_address must be a string");
        assert!(body["permit2_address"].is_string(), "permit2_address must be a string");
    }

    #[actix_web::test]
    async fn test_info_returns_correct_permit2_address() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/info", web::get().to(super::info)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/info")
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

        let addr = body["permit2_address"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert!(
            addr.contains("000000000022d473030f116ddee9f6b43ac78ba3"),
            "expected canonical Permit2 address, got {addr}"
        );
    }

    #[actix_web::test]
    async fn test_info_returns_correct_router_address() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/info", web::get().to(super::info)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/info")
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

        let addr = body["router_address"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert!(
            addr.contains("fd0b31d2e955fa55e3fa641fe90e08b677188d35"),
            "expected Ethereum Tycho Router address, got {addr}"
        );
    }

    #[actix_web::test]
    async fn test_info_response_includes_version() {
        let state = make_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .route("/v1/info", web::get().to(super::info)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/v1/info")
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }
}
