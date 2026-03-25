# fynd-rpc

HTTP RPC server for the Fynd DEX router. Wraps `fynd-core` with Actix Web and adds HTTP
infrastructure.

## Module Map

| Module | Description |
|---|---|
| `builder.rs` | `FyndRPCBuilder` wraps `FyndBuilder`, adds HTTP server config. `FyndRPC` struct runs the server with graceful shutdown |
| `config.rs` | `WorkerPoolsConfig` (TOML loader), `BlacklistConfig`, `defaults` module re-exporting `fynd-core` defaults + HTTP-specific ones |
| `protocols.rs` | `fetch_protocol_systems()` — paginated Tycho RPC call to discover available protocols |
| `api/` | HTTP endpoint handlers and OpenAPI documentation |

## Features

| Feature | Effect |
|---|---|
| `experimental` | Enables `GET /v1/prices` endpoint and derived data access in `AppState` |

## API Endpoints

| Endpoint | Handler | Description |
|---|---|---|
| `POST /v1/quote` | `handlers::quote` | Submit orders, receive optimal routes |
| `GET /v1/health` | `handlers::health` | Health check (data freshness, derived data readiness, pool count) |
| `GET /v1/prices` | `handlers::get_prices` | Token prices, spot prices, pool depths (experimental feature only) |

## API Module (`api/`)

| File | Purpose |
|---|---|
| `mod.rs` | `configure_app()`, `AppState`, `HealthTracker`, `ApiDoc` (utoipa OpenAPI) |
| `handlers.rs` | Request handlers for `/v1/quote` and `/v1/health` |
| `dto.rs` | Re-exports wire types from `fynd-rpc-types` (conversions to `fynd-core` types live in `fynd-rpc-types` via the `core` feature) |
| `error.rs` | `ApiError` type with HTTP status code mapping |
| `prices.rs` | Types and helpers for `GET /v1/prices`: query params, response DTOs (`PricesResponse`, `TokenPriceEntry`, etc.), `price_to_f64` conversion |

## Builder Pattern

`FyndRPCBuilder` delegates all solver configuration to `FyndBuilder` and adds:
- `http_host` / `http_port` (defaults: `0.0.0.0:3000`)
- `gas_price_stale_threshold` (health returns 503 when exceeded)

The builder calls `FyndBuilder::build()` → `Solver::into_parts()` → wraps the router in
`AppState` → starts an Actix `HttpServer`.

## Defaults

The `config::defaults` module re-exports `fynd-core::solver::defaults::*` and adds HTTP-specific
constants:
- `HTTP_HOST = "0.0.0.0"`, `HTTP_PORT = 3000`
- `DEFAULT_RPC_URL = "https://eth.llamarpc.com"`
- `WORKER_ROUTER_TIMEOUT_MS = 100` (tighter than fynd-core's 10s standalone default)
- `default_tycho_url(chain)` maps chain names to hosted endpoints
