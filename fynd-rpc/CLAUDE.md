# fynd-rpc

HTTP RPC server for the Fynd DEX router. Wraps `fynd-core` with Actix Web and adds HTTP
infrastructure.

## Module Map

| Module | Description |
|---|---|
| `builder.rs` | `FyndRPCBuilder` wraps `FyndBuilder`, adds HTTP server config. `FyndRPC` struct runs the server with graceful shutdown |
| `config.rs` | `WorkerPoolsConfig` (TOML loader), `BlocklistConfig`, `defaults` module re-exporting `fynd-core` defaults + HTTP-specific ones |
| `protocols.rs` | `fetch_protocol_systems()` — Tycho RPC call to discover available protocols; `resolve_protocols()` — higher-level wrapper used by `serve` and `scale` that parses each explicit entry into a `ProtocolSpec` (before the RPC call, so a bad `exclusive:` prefix fails fast), expands `all_onchain`/`native_onchain` tokens, merges the two by protocol system — one entry per system, exclusive winning over public regardless of order — and finally drops every system named with an `exclude:` entry (parsed through `ProtocolSpec` so `exclude:exclusive:x` and `exclude:x` both name system `x`; a protocol both requested and excluded, an exclusion naming nothing, and an exclusion matching no streamed protocol are all errors) |
| `api/` | HTTP endpoint handlers and OpenAPI documentation |

## Features

| Feature | Effect |
|---|---|
| `experimental` | Enables the `GET /v1/prices` and `GET /v1/tokens` endpoints plus derived/market data access in `AppState` |

## API Endpoints

| Endpoint | Handler | Description |
|---|---|---|
| `POST /v1/quote` | `handlers::quote` | Submit orders, receive optimal routes. The `x-exclusive-access: true` request header (set by the authenticating proxy, never by the caller) allocates exclusive-access worker pools to the request; any other value or none restricts it to public pools. The `x-disable-slippage-taking: true` request header (same proxy-only trust model) makes the encoder attach server-signed zero-fee `ClientFeeParams` so the FeeCalculator applies the signer's positive-slippage exemption; it is rejected when the request body also carries `client_fee_params`. Both headers are only meaningful when the server is unreachable except through that proxy. Composed of the public `validate_quote_request` / `apply_disable_slippage_taking` / `ReplayRequest::capture` / `log_quote_outcome` helpers, for embedders writing their own variant |
| `GET /v1/health` | `handlers::health` | Health check (data freshness, derived data readiness, gas-price staleness, solver pool count). Returns 503 when market data is stale, derived data is not ready, or the gas price is stale |
| `GET /v1/info` | `handlers::info` | Static metadata about this Fynd instance (version, chain, spender address) |
| `GET /v1/prices` | `handlers::get_prices` | Token prices, spot prices, component depths (experimental feature only) |
| `GET /v1/tokens` | `handlers::get_tokens` | Graph tokens with metadata and liquidity/degree ranking, lazily cached per derived-data update (experimental feature only) |

## API Documentation

Up to two Swagger UIs are served, both built from the same `ApiDoc` annotations:

| Path | Spec | Describes |
|---|---|---|
| `/docs/` | `/api-docs/openapi.json` | Self-hosted deployments: `/v1/quote` on the origin it is reached at, no authentication. Always served |
| `/docs/hosted/` | `/api-docs/hosted/openapi.json` | The hosted gateway: `/v1/{chain}/quote` with a `chain` path parameter and an API key sent as the raw `Authorization` header value. Only served when a gateway URL is set via `--hosted-swagger-url` / `FYND_HOSTED_SWAGGER_URL` |

`api/docs.rs` derives the hosted spec from the self-hosted one at startup, so endpoint
annotations live in one place.

## API Module (`api/`)

| File | Purpose |
|---|---|
| `mod.rs` | `configure_app()`, `AppState`, `HealthTracker`, `ApiDoc` (utoipa OpenAPI) |
| `docs.rs` | Builds the self-hosted and hosted OpenAPI specs and registers both Swagger UIs |
| `handlers.rs` | Request handlers for `/v1/quote`, `/v1/health`, and `/v1/info` |
| `dto.rs` | Re-exports wire types from `fynd-rpc-types` (conversions to `fynd-core` types live in `fynd-rpc-types` via the `core` feature) |
| `error.rs` | `ApiError` type with HTTP status code mapping |
| `exclusive_access.rs` | Reads the `x-exclusive-access` header into `fynd_core::ExclusiveAccess` |
| `prices.rs` | Types and helpers for `GET /v1/prices`: query params, response DTOs (`PricesResponse`, `TokenPriceEntry`, etc.), `price_to_decimal_string` exact decimal serialization |
| `tokens.rs` | Types and helpers for `GET /v1/tokens`: `TokensResponse`/`GraphTokenEntry` DTOs, `build_token_entries` ranking fold, `TokensCache` |
| `middleware.rs` | HTTP metrics middleware: records `http_request_duration_seconds` (histogram) and `http_requests_total` (counter with per-client `user_identity`/`user_plan`/`client_version` labels sourced from proxy-injected headers) |

## Builder Pattern

`FyndRPCBuilder` delegates all solver configuration to `FyndBuilder` and adds:
- `http_host` / `http_port` (defaults: `0.0.0.0:3000`)
- `gas_price_stale_threshold` (health returns 503 when exceeded)
- `price_guard_enabled(bool)` (delegates to `FyndBuilder`; default `false`)
- `configure_routes(f)` registers a caller's routes inside the `/v1` scope ahead of the defaults, so
  a binary embedding `fynd-rpc` (e.g. the hosted service) can shadow one endpoint and keep the rest

The builder calls `FyndBuilder::build()` → `Solver::into_parts()` → wraps the router in
`AppState` → starts an Actix `HttpServer`.

## Defaults

The `config::defaults` module re-exports `fynd-core::solver::defaults::*` and adds HTTP-specific
constants:
- `HTTP_HOST = "0.0.0.0"`, `HTTP_PORT = 3000`
- `WORKER_ROUTER_TIMEOUT_MS = 100` (tighter than fynd-core's 10s standalone default)
- `default_tycho_url(chain)` maps chain names to hosted endpoints
- `default_rpc_url(chain)` maps chain names to public JSON-RPC endpoints
