# fynd-rpc

HTTP RPC server for the Fynd DEX router. Wraps `fynd-core` with Actix Web and adds HTTP
infrastructure.

## Module Map

| Module | Description |
|---|---|
| `builder.rs` | `FyndRPCBuilder` wraps `FyndBuilder`, adds HTTP server config. `FyndRPC` struct runs the server with graceful shutdown |
| `config.rs` | `WorkerPoolsConfig` (TOML loader), `BlocklistConfig`, `defaults` module re-exporting `fynd-core` defaults + HTTP-specific ones |
| `protocols.rs` | `fetch_protocol_systems()` — Tycho RPC call to discover available protocols; `resolve_protocols()` — higher-level wrapper used by `serve` and `scale` that parses each explicit entry into a `ProtocolSpec` (before the RPC call, so a bad `exclusive:` prefix fails fast), expands `all_onchain`/`native_onchain` tokens, merges the two by protocol system — one entry per system, exclusive winning over public regardless of order — drops every system named with an `exclude:` entry (parsed through `ProtocolSpec` so `exclude:exclusive:x` and `exclude:x` both name system `x`; a protocol both requested and excluded and an exclusion naming nothing are errors; an exclusion matching no streamed protocol is only a warning), and finally drops every resolved system Tycho does not serve with a warning. The protocol systems are fetched whenever any entry names a Tycho system, not only for the expansion tokens, so the availability check has something to check against; a list of `rfq:`/`pricelevelstream:` entries only skips the fetch and needs no reachable Tycho |
| `api/` | HTTP endpoint handlers and OpenAPI documentation |

## Features

| Feature | Effect |
|---|---|
| `experimental` | Enables the `GET /v1/prices` and `GET /v1/tokens` endpoints plus derived/market data access in `AppState` |

## API Endpoints

| Endpoint | Handler | Description |
|---|---|---|
| `POST /v1/quote` | `handlers::quote` | Submit orders, receive optimal routes. The `x-exclusive-access: true` request header (set by the authenticating proxy, never by the caller) allocates exclusive-access worker pools to the request; any other value or none restricts it to public pools. The `x-disable-slippage-taking: true` request header (same proxy-only trust model) makes the encoder attach server-signed zero-fee `ClientFeeParams` so the FeeCalculator applies the signer's positive-slippage exemption — which also makes the signer the request's fee client in place of any other attribution, and gives the quote a ~120s deadline it did not have before. Both headers are only meaningful when the server is unreachable except through that proxy. Composed of the public `validate_quote_request` / `disable_slippage_taking::apply` / `RequestRecord::capture` / `QuoteRecord::build` / `log_quote_outcome` helpers, for embedders writing their own variant |
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
| `disable_slippage_taking.rs` | Reads the `x-disable-slippage-taking` header (`from_headers`) and writes it onto the request's `EncodingOptions` (`apply`), in both directions — the header is the only thing that can turn the encoding on |
| `record.rs` | `QuoteRecord`, the record every answered `/v1/quote` produces for the collector (fynd-hosted-service `crates/collector`, `POST /v1/records`): chain, `served_at`, `schema_version`, the client block, the request (`RequestRecord`: the `ReplayRequest` capture plus the non-secret encoding options, absent when the request asked for no encoding) and the outcome (per-order status, `failure_reason` slug, amounts, algorithm, route; request-level gas price and block). Built on success and failure alike; a request-level error repeats its lowercased code as every order's status. `block` is the block a route priced against; when nothing priced, the handler falls back to the pod's Tycho head and `block_source` says which of the two it is (`order` / `head`). The replay log reads its request and outcome back off the record (`replay`, `log_outcome`), so the log line and the record cannot disagree about one quote. An allowlist by construction, like `request_capture.rs` |
| `record_emitter.rs` | The record pipeline, both ends. `RecordEmitter` is the bounded `tokio::sync::mpsc` queue the quote handler pushes each record onto with `try_send`: it never blocks and never fails the quote, and a record arriving at a full queue is dropped — the incoming one, never an older one, so an outage leaves a clean prefix of the traffic rather than a gapped sample. `record_sink(url)` builds the queue and spawns the task draining it, or nothing at all when no collector is configured; the task batches records (capped by count and by bytes) and POSTs `{"records": [...]}` to `<url>/v1/records`, zstd-compressed, at least once a second, with a 2 s timeout. A batch is never retried — the collector mints the record ids, so a retry duplicates every record in it. Drops are counted in `quote_records_dropped_total{reason}`: `queue_full`, `sender_stopped`, `sink_timeout`, `sink_rejected` |
| `request_capture.rs` | `ReplayRequest`, the routing-essential capture of a request (orders, solve options, route filter, the two proxy-header flags) that the `quote_failure` and `slow_solve` log lines carry, plus the `quote_status_code` / `failure_reason_slug` / `request_failure_slug` vocabularies the record shares. Its field names are also the request half of the record's wire format, so renaming one needs a `record::SCHEMA_VERSION` bump. `RequestOutcome` is the log's view of a solve, built from the record rather than from the solve result |
| `prices.rs` | Types and helpers for `GET /v1/prices`: query params, response DTOs (`PricesResponse`, `TokenPriceEntry`, etc.), `price_to_decimal_string` exact decimal serialization |
| `tokens.rs` | Types and helpers for `GET /v1/tokens`: `TokensResponse`/`GraphTokenEntry` DTOs, `build_token_entries` ranking fold, `TokensCache` |
| `middleware.rs` | HTTP metrics middleware: records `http_request_duration_seconds` (histogram) and `http_requests_total` (counter with per-client `user_identity`/`user_plan`/`client_version` labels sourced from proxy-injected headers). A `User-Identity` value outside `[A-Za-z0-9._/-]{1,64}` is slugified byte by byte (`Relay - FOMO` → `Relay---FOMO`, cut at 64 bytes) so the client keeps its own series; an empty value is `invalid`, an absent header is `unknown`. `ClientInfo` holds both forms — the slugs for the metric labels, and what the client actually sent for the quote record's `client` block — and the middleware stashes it in the request extensions so the quote handler reads the headers once per request |

## Builder Pattern

`FyndRPCBuilder` delegates all solver configuration to `FyndBuilder` and adds:
- `http_host` / `http_port` (defaults: `0.0.0.0:3000`)
- `gas_price_stale_threshold` (health returns 503 when exceeded)
- `price_guard_enabled(bool)` (delegates to `FyndBuilder`; default `false`)
- `configure_routes(f)` registers a caller's routes inside the `/v1` scope ahead of the defaults, so
  a binary embedding `fynd-rpc` (e.g. the hosted service) can shadow one endpoint and keep the rest
- `record_sink_url(Option<String>)` (`--record-sink-url` / `RECORD_SINK_URL`): the collector's root
  URL. Unset by default, and then no queue and no sending task are built at all. A value that is not
  an `http`/`https` URL fails the build rather than dropping every record at runtime

The builder calls `FyndBuilder::build()` → `Solver::into_parts()` → wraps the router in
`AppState` → starts an Actix `HttpServer`.

## Defaults

The `config::defaults` module re-exports `fynd-core::solver::defaults::*` and adds HTTP-specific
constants:
- `HTTP_HOST = "0.0.0.0"`, `HTTP_PORT = 3000`
- `WORKER_ROUTER_TIMEOUT_MS = 100` (tighter than fynd-core's 10s standalone default)
- Record pipeline: `RECORD_QUEUE_CAPACITY = 5_000`, `RECORD_BATCH_MAX_RECORDS = 1_000`,
  `RECORD_BATCH_MAX_BYTES = 4 MiB` (uncompressed, against the collector's 32 MiB body limit),
  `RECORD_FLUSH_INTERVAL = 1s`, `RECORD_SINK_TIMEOUT = 2s`
- `default_tycho_url(chain)` maps chain names to hosted endpoints
- `default_rpc_url(chain)` maps chain names to public JSON-RPC endpoints
