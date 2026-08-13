//! HTTP request handlers for the solver API.

use actix_web::{web, HttpRequest, HttpResponse};
use tracing::instrument;
#[cfg(feature = "experimental")]
use tracing::{debug, info, warn};

use super::{dto, ApiError, AppState};
#[cfg(feature = "experimental")]
use crate::api::prices::{
    price_to_decimal_string, ComponentDepthEntry, ComputationBlocks, IncludeField, PricesQuery,
    PricesResponse, SpotPriceEntry, TokenPriceEntry,
};
#[cfg(feature = "experimental")]
use crate::api::tokens::{build_token_entries, TokensCache, TokensQuery, TokensResponse};
use crate::api::{
    error::{solve_error_code, ErrorResponse},
    exclusive_access,
    request_capture::{
        self, failure_reason_slug, log_request_capture, log_slow_solve, quote_status_code,
        RequestOutcome,
    },
};

/// Configures API routes under /v1 namespace.
pub(crate) fn configure_routes(cfg: &mut web::ServiceConfig) {
    let scope = web::scope("/v1")
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
pub(crate) async fn quote(
    state: web::Data<AppState>,
    request: web::Json<dto::QuoteRequest>,
    http_request: HttpRequest,
) -> Result<HttpResponse, ApiError> {
    let access = exclusive_access::from_headers(http_request.headers());
    let dto_request = request.into_inner();

    // Validate request
    if dto_request.orders().is_empty() {
        return Err(ApiError::BadRequest("no orders provided".to_string()));
    }

    // Capture a re-issuable, signature-free copy of the request BEFORE the core
    // conversion consumes `dto_request`. This is cheap (no serialization); the
    // JSON encoding is deferred to the failure-only task below.
    let num_orders = dto_request.orders().len();
    let replay_capture = request_capture::ReplayRequest::capture(&dto_request, access);

    // Convert DTO to core types
    let core_request: fynd_core::QuoteRequest = dto_request.into();

    // Validate orders (unchanged from the original handler).
    for order in core_request.orders() {
        if let Err(e) = order.validate() {
            return Err(ApiError::BadRequest(format!("invalid order {}: {}", order.id(), e)));
        }
    }

    let result = state
        .worker_router()
        .quote(core_request, access)
        .await;

    let outcome = match &result {
        Ok(core_quote) => RequestOutcome::Solved {
            solve_time_ms: core_quote.solve_time_ms(),
            order_statuses: core_quote
                .orders()
                .iter()
                .map(|order_quote| quote_status_code(order_quote.status()))
                .collect(),
            failure_reasons: core_quote
                .orders()
                .iter()
                .map(|order_quote| {
                    failure_reason_slug(order_quote.status(), order_quote.no_route_cause())
                })
                .collect(),
        },
        Err(error) => RequestOutcome::Failed { code: solve_error_code(error) },
    };
    // Only failed quotes get a capture line (successful quotes are the common
    // case and would dominate log volume; see RequestOutcome::is_failure), and
    // successful solves above the slow threshold get a slow_solve line. Both
    // are emitted from a detached task so serialization never adds latency to
    // the response; the current tracing span is carried over so the lines keep
    // their request context.
    let slow_solve_time_ms = match &outcome {
        RequestOutcome::Solved { solve_time_ms, .. }
            if *solve_time_ms > request_capture::SLOW_SOLVE_THRESHOLD_MS =>
        {
            Some(*solve_time_ms)
        }
        RequestOutcome::Solved { .. } | RequestOutcome::Failed { .. } => None,
    };
    if outcome.is_failure() || slow_solve_time_ms.is_some() {
        let span = tracing::Span::current();
        actix_web::rt::spawn(async move {
            span.in_scope(|| {
                let replay_json = replay_capture.to_json();
                if outcome.is_failure() {
                    log_request_capture(num_orders, &replay_json, &outcome);
                }
                if let Some(solve_time_ms) = slow_solve_time_ms {
                    log_slow_solve(
                        solve_time_ms,
                        num_orders,
                        request_capture::SLOW_SOLVE_THRESHOLD_MS,
                        &replay_json,
                    );
                }
            });
        });
    }

    let core_quote = result?;

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
pub(crate) async fn health(state: web::Data<AppState>) -> HttpResponse {
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
pub(crate) async fn info(state: web::Data<AppState>) -> HttpResponse {
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
/// GET /v1/prices - Return derived token prices and optional market data.
///
/// By default returns token gas prices only. Each `prices[].price` is a plain decimal string
/// holding raw target-token units divided by raw gas-token units; consumers must normalize
/// both tokens' decimals before using it. Use `include` query parameter to add spot prices
/// and/or component depths.
///
/// # Query Parameters
///
/// - `include` - Comma-separated list: `depths`, `spot_prices`
/// - `limit` - Max entries for spot_prices / component_depths (default: 1000)
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
    let token_prices_block = store
        .token_prices_block()
        .ok_or(ApiError::StaleData { age_ms: u64::MAX })?;
    if want_spot && store.spot_prices_block().is_none() {
        return Err(ApiError::StaleData { age_ms: u64::MAX });
    }
    if want_depths && store.component_depths_block().is_none() {
        return Err(ApiError::StaleData { age_ms: u64::MAX });
    }
    let spot_prices_block = store.spot_prices_block();
    let component_depths_block = store.component_depths_block();
    let token_prices = store.token_prices().cloned();
    let spot_prices_data = if want_spot { store.spot_prices().cloned() } else { None };
    let component_depths_data = if want_depths { store.component_depths().cloned() } else { None };
    drop(store);

    // Convert token gas prices
    let mut prices = Vec::new();
    let mut skipped_tokens = 0usize;
    if let Some(token_prices) = &token_prices {
        for (address, price) in token_prices {
            match price_to_decimal_string(&price.numerator, &price.denominator) {
                Some(price) => {
                    prices.push(TokenPriceEntry { token: address.clone(), price });
                }
                None => {
                    debug!(
                        token = %address,
                        "cannot serialize token price (zero or oversized numerator/denominator)"
                    );
                    skipped_tokens += 1;
                }
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

    // Convert component depths if requested (sorted for deterministic limit)
    let component_depths = if want_depths {
        let mut entries: Vec<ComponentDepthEntry> = component_depths_data
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
        blocks: ComputationBlocks {
            token_prices: token_prices_block,
            spot_prices: spot_prices_block,
            component_depths: component_depths_block,
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
/// - `limit` - Maximum number of tokens returned (default: 1000)
/// - `offset` - Number of tokens to skip from the start of the ranked list (default: 0)
#[utoipa::path(
    get,
    path = "/v1/tokens",
    tag = "tokens",
    params(TokensQuery),
    responses(
        (status = 200, description = "Graph tokens returned", body = TokensResponse),
        (status = 503, description = "Data not yet available", body = ErrorResponse),
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
    let offset = query.offset.unwrap_or(0);

    let cache_key = {
        let store = state.derived_data.read().await;
        let token_prices_block = store
            .token_prices_block()
            .ok_or(ApiError::StaleData { age_ms: u64::MAX })?;
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
        let token_prices_block = store
            .token_prices_block()
            .ok_or(ApiError::StaleData { age_ms: u64::MAX })?;
        (
            (token_prices_block, store.component_depths_block()),
            store.token_prices().cloned(),
            store.component_depths().cloned(),
        )
    };

    // Snapshot under the read guard and rank outside it, so the per-block feed writer
    // is never blocked by the fold over the full topology.
    let (topology, token_registry) = {
        let market = state.market_data.read().await;
        (market.component_topology(), market.token_registry_ref().clone())
    };
    let entries =
        build_token_entries(&topology, &token_registry, depths.as_ref(), token_prices.as_ref());

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

    use crate::api::{dto::QuoteRequest, AppState, HealthTracker};

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
            1,
            Some(router_address),
            permit2_address,
            #[cfg(feature = "experimental")]
            derived_data,
            #[cfg(feature = "experimental")]
            tycho_simulation::tycho_common::models::Address::from([0u8; 20]),
            #[cfg(feature = "experimental")]
            market_data,
        )
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
        let mut token_prices = std::collections::HashMap::new();
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
        let body: Value = test::call_and_read_body_json(&app, request).await;

        assert_eq!(body["blocks"]["token_prices"], 19_000_000);
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
        let mut token_prices = std::collections::HashMap::new();
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
        // USDC: 400 raw units deep at price 2 = 800 gas units; WETH: 100 at 1 = 100.
        assert_eq!(tokens[0]["symbol"], "USDC");
        assert_eq!(tokens[0]["liquidity"], 800.0);
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
