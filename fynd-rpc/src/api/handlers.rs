//! HTTP request handlers for the solver API.

use actix_web::{web, HttpResponse};
use tracing::instrument;
#[cfg(feature = "experimental")]
use tracing::{info, warn};

use super::{dto, ApiError, AppState};
#[cfg(feature = "experimental")]
use crate::api::prices::{
    price_to_decimal_string, ComponentDepthEntry, ComputationBlocks, IncludeField, PricesQuery,
    PricesResponse, SpotPriceEntry, TokenPriceEntry, PRICE_UNIT_CONTRACT_V1,
    RAW_TOKEN_UNITS_PER_RAW_GAS_UNIT,
};
use crate::api::{
    error::{solve_error_code, ErrorResponse},
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
#[instrument(skip(state, request), fields(num_orders = request.orders().len()))]
pub(crate) async fn quote(
    state: web::Data<AppState>,
    request: web::Json<dto::QuoteRequest>,
) -> Result<HttpResponse, ApiError> {
    let dto_request = request.into_inner();

    // Validate request
    if dto_request.orders().is_empty() {
        return Err(ApiError::BadRequest("no orders provided".to_string()));
    }

    // Capture a re-issuable, signature-free copy of the request BEFORE the core
    // conversion consumes `dto_request`. This is cheap (no serialization); the
    // JSON encoding is deferred to the failure-only task below.
    let num_orders = dto_request.orders().len();
    let replay_capture = request_capture::ReplayRequest::capture(&dto_request);

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
        .quote(core_request)
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
/// By default returns token gas prices only. Each `prices[].price` follows
/// `PRICE_UNIT_CONTRACT_V1`: `rawTokenUnitsPerRawGasUnit` (raw target-token units divided by raw
/// gas-token units). Use `include` query parameter to add spot prices and/or component depths.
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
    if let Some(token_prices) = &token_prices {
        for (address, price) in token_prices {
            match price_to_decimal_string(&price.numerator, &price.denominator) {
                Some(s) => {
                    prices.push(TokenPriceEntry { token: address.clone(), price: s });
                }
                None => {
                    warn!(
                        token = %address,
                        "skipping token with non-finite, non-positive, or unrepresentable price"
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
        contract_version: PRICE_UNIT_CONTRACT_V1,
        price_unit: RAW_TOKEN_UNITS_PER_RAW_GAS_UNIT,
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
        )
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_matches_canonical_unit_contract_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/v1-prices-unit-contract-v1.json"
        ))
        .unwrap();
        let mut state = make_test_state();
        state.gas_token = tycho_simulation::tycho_common::models::Address::from_str(
            fixture["gasToken"]["address"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let mut token_prices = std::collections::HashMap::new();

        for case in fixture["cases"].as_array().unwrap() {
            if case["expectation"] != "accepted" {
                continue;
            }
            let address = tycho_simulation::tycho_common::models::Address::from_str(
                case["token"]["address"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            let fraction = &case["priceFraction"];
            let numerator =
                num_bigint::BigUint::from_str(fraction["numerator"].as_str().unwrap()).unwrap();
            let denominator = num_bigint::BigUint::from_str(
                fraction["denominator"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            token_prices.insert(address, Price::new(numerator, denominator));
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
        assert_eq!(body["contract_version"], "PRICE_UNIT_CONTRACT_V1");
        assert_eq!(body["price_unit"], "raw_token_units_per_raw_gas_unit");
        assert!(body["gas_token"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case(
                fixture["gasToken"]["address"]
                    .as_str()
                    .unwrap()
            ));
        let response_prices = body["prices"].as_array().unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            if case["expectation"] != "accepted" {
                continue;
            }
            let expected_address = case["token"]["address"]
                .as_str()
                .unwrap();
            let response_entry = response_prices
                .iter()
                .find(|entry| {
                    entry["token"]
                        .as_str()
                        .unwrap()
                        .eq_ignore_ascii_case(expected_address)
                })
                .unwrap_or_else(|| panic!("missing handler response for {}", case["name"]));
            assert_eq!(
                response_entry["price"]
                    .as_str()
                    .unwrap(),
                case["rawPrice"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[cfg(feature = "experimental")]
    #[actix_web::test]
    async fn test_prices_handler_skips_non_serializable_prices() {
        let state = make_test_state();
        let mut token_prices = std::collections::HashMap::new();
        let valid = tycho_simulation::tycho_common::models::Address::from([1u8; 20]);
        token_prices.insert(valid.clone(), Price::new(1u8.into(), 2u8.into()));
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
