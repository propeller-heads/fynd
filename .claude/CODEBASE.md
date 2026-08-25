<!-- docs-synced-at: 4a30359cea111f5590fe698357b3a707fee65ece -->
# Fynd Codebase Guide

High-performance DeFi route-finding engine built on Tycho. Finds optimal swap routes across
multiple DeFi protocols in real-time.

## What is Fynd

Fynd is a solver that indexes live DEX liquidity via Tycho's streaming API, maintains an in-memory
graph of token pairs and components (liquidity pools), and runs pluggable routing algorithms on dedicated OS threads to
find optimal swap paths. It exposes an HTTP RPC for quote requests and returns the best
gas-aware solution with optional on-chain transaction encoding.

Key properties:
- **Multi-protocol**: Routes through any on-chain protocol supported by Tycho, plus RFQ protocols
- **Real-time**: Tycho Stream keeps all component states synchronized every block
- **Multi-algorithm competition**: Multiple worker pools compete in parallel; best result wins
- **Gas-aware**: Best solution selected by net output after gas costs
- **Extensible**: Implement the `Algorithm` trait to add new routing strategies

## Workspace Module Map

### Core Crates

| Crate | Location | Description |
|---|---|---|
| `fynd` | root (`src/`) | CLI binary and library crate: parses args, sets up observability, runs `FyndRPCBuilder`. `lib.rs` re-exports `fynd_core` and `fynd_rpc` as a single dependency |
| [`fynd-core`](../fynd-core/CLAUDE.md) | `fynd-core/` | Pure solving logic: algorithms, worker pools, graph, feed, derived data, encoding. No HTTP deps |
| [`fynd-rpc`](../fynd-rpc/CLAUDE.md) | `fynd-rpc/` | HTTP RPC server builder (Actix Web): API handlers, middleware, `FyndRPCBuilder` |
| [`fynd-rpc-types`](../fynd-rpc-types/CLAUDE.md) | `fynd-rpc-types/` | Shared DTO types for the RPC API (request/response wire format) |
| `fynd-test-fixtures` | `test-fixtures/` | Shared types for recorded-market test fixtures: `MarketRecording`, expected outputs, test scenarios. Not published |

### [Clients](../clients/CLAUDE.md)

Both clients wrap the same OpenAPI spec (`clients/openapi.json`, generated via `cargo run -- openapi`).

| Client | Location | Package |
|---|---|---|
| Rust | `clients/rust/` | `fynd-client` (Cargo workspace member) |
| TypeScript | `clients/typescript/` | `@kayibal/fynd-client` (pnpm workspace) |

### [Tools](../tools/CLAUDE.md)

| Tool | Location | Description |
|---|---|---|
| `fynd-benchmark` | `tools/benchmark/` | Load testing, solver comparison, trade dataset download |
| `fynd-swap-cli` | `tools/fynd-swap-cli/` | Quote and execute token swaps (ERC-20 or Permit2) |
| `hindsight` | `tools/hindsight/` | Decode solver swaps from on-chain data; live-monitor re-solve quality |
| `record-market` | `tools/record-market/` | Record live Tycho market state and generate expected outputs for the integration tests |
| `fynd-gas-audit` | `tools/fynd-gas-audit/` | Compare quote-time gas estimates against `eth_estimateGas` |
| `erc20-overrides` | `tools/erc20-overrides/` | ERC-20 storage slot detection for dry-run storage overrides |
| `fynd-tools-common` | `tools/common/` | Shared internal library for tool crates |

## Architecture Overview

See `docs/ARCHITECTURE.md` for the full architecture diagram and detailed component descriptions.

### Core Components

1. **RouterApi** (`fynd-rpc/src/api/`) — Actix Web HTTP handlers: `POST /v1/quote`, `GET /v1/health`, `GET /v1/info`
2. **WorkerPoolRouter** (`fynd-core/src/worker_pool_router/`) — Allocates the pools that serve each order, fans out to those, selects best by `amount_out_net_gas`
3. **WorkerPool** (`fynd-core/src/worker_pool/`) — N `SolverWorker` instances on dedicated OS threads per pool
4. **Algorithm trait** (`fynd-core/src/algorithm/`) — Pluggable route-finding; built-in:
   `MostLiquidAlgorithm`, `BellmanFordAlgorithm`, `PathFrankWolfeAlgorithm`, and
   `WaterFillAlgorithm`
5. **MarketState** (`fynd-core/src/feed/market_data.rs`) — `Arc<RwLock<>>` of all component/token/gas state; accessed via `MarketData` handle
6. **TychoFeed** (`fynd-core/src/feed/tycho_feed.rs`) — Background task: Tycho WebSocket → MarketState → broadcast events
7. **Derived Data** (`fynd-core/src/derived/`) — Pre-computed spot prices, component (pool) depths, token gas prices
8. **Encoding** (`fynd-core/src/encoding/`) — Encodes solved routes into on-chain transactions via `TychoEncoder`
9. **Graph** (`fynd-core/src/graph/`) — `GraphManager` trait + `PetgraphStableDiGraphManager` implementation

### Data Flow

**Market update path** (continuous, every block):
1. `TychoFeed` receives state updates from Tycho WebSocket
2. Writes new component/token/state data into `MarketState` (write lock)
3. Broadcasts `MarketEvent` → each `SolverWorker` updates its local graph via `GraphManager`
4. `GasPriceFetcher` runs independently on a timer → fetches gas price from RPC node → writes to `MarketState`
5. Triggers the `ComputationManager` → runs spot prices, token gas prices, then component (pool) depths → broadcasts `DerivedDataEvent` → workers update edge weights. Token pricing's per-token sell solves can delay a block's spot prices and depths — an accepted trade-off

**Quote request path** (`POST /v1/quote`):
1. `RouterApi` validates the request
2. `WorkerPoolRouter` allocates the worker pools serving each order (an exclusive-access pool only for a request granted access via the `x-exclusive-access` header) and fans out to them in parallel
3. Each pool's `TaskQueue` dispatches to a `SolverWorker` on a dedicated OS thread
4. Worker calls `Algorithm::find_best_route` with its local graph + shared market/derived data
5. `WorkerPoolRouter` collects results, ranks candidates by `amount_out_net_gas` descending; if price guard is enabled it validates in rank order
6. If `EncodingOptions` provided, `Encoder` produces ABI-encoded calldata
7. Returns `Quote` response

### Threading Model

- **Actix/Tokio runtime** (async I/O): HTTP server, TychoFeed, WorkerPoolRouter, gas fetcher, ComputationManager
- **Worker pools** (dedicated OS threads): Each `SolverWorker` has a local graph and single-thread tokio runtime
- **Communication**: `async_channel` (worker pool queues), `oneshot` (responses), `broadcast` (events), `Arc<RwLock<>>` (shared data)

## Configuration

### Environment Variables

| Variable | Purpose |
|---|---|
| `TYCHO_API_KEY` | Tycho API key (optional) |
| `RPC_URL` | Ethereum RPC endpoint (chain-specific default) |
| `TYCHO_URL` | Tycho endpoint (chain-specific default) |
| `HTTP_HOST` | HTTP bind address (default: `0.0.0.0`) |
| `HTTP_PORT` | API port (default: `3000`) |
| `WORKER_POOLS_CONFIG` | Worker pools config file (default: `worker_pools.toml`) |
| `BLOCKLIST_CONFIG` | Blocklist config file |
| `EXCLUSIVE_SWAP_CONTROLLER_KEY` | Restricted exclusive-liquidity deployments only — see [Exclusive liquidity](#exclusive-liquidity-restricted). Unset in ordinary deployments |
| `CALLDATA_WATERMARK` | Watermark bytes appended to every encoded transaction's calldata (also `--calldata-watermark`). Ignored by the EVM; attributes router calls to this deployment. Unset by default |
| `RUST_LOG` | Tracing filter (e.g. `info,fynd=debug`) |
| `METRICS_PORT` | Prometheus metrics server port (default: `9898`, requires `metrics` feature) |
| `FYND_HOSTED_SWAGGER_URL` | Server URL advertised by the hosted OpenAPI spec. When unset, the hosted Swagger UI (`/docs/hosted/`) is not served — only the self-hosted `/docs/` |

### CLI Commands

| Command | Purpose |
|---|---|
| `serve` | Run the solver: Tycho feed + HTTP RPC server. Notable flags: `--enable-price-guard` (default `false`), `--partial-blocks` (enable flashblock/partial-block updates from Tycho stream) |
| `openapi` | Print the OpenAPI spec JSON to stdout |
| `derive-connector-tokens` | Derive and print connector token lists for configured protocols |

### Config Files

| File | Purpose |
|---|---|
| `worker_pools.toml` | Worker pool definitions: algorithm, num_workers, hop limits, timeout, `exclude_protocols` (protocol systems that worker pool never routes through). Optional — binary falls back to embedded defaults if not found |
| `blocklist.toml` | Component IDs to exclude from the Tycho stream. Optional — falls back to tycho-simulation defaults if not found |

## Testing

- `cargo nextest run --workspace --all-targets --all-features` — full test suite
- `cargo nextest run -p fynd-core --features test-utils --test integration` — replayed-market integration tests against recorded fixtures (`fynd-core/tests/fixtures/`, Git LFS). See `fynd-core/tests/integration/README.md`; regenerate baselines with the ignored `regenerate_expected_outputs` test, record fresh markets with `tools/record-market`
- `cargo +nightly clippy --workspace --all-targets --all-features` — lint
- `cargo +nightly fmt --all --check` — format check
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked --package fynd-core --package fynd-rpc-types --package fynd-rpc --package fynd-client` — doc build (broken links, missing docs)
- OpenAPI drift: `cargo run -- openapi | jq 'del(.info.version)'` vs `clients/openapi.json`
- TypeScript: `pnpm --dir clients/typescript install && pnpm --dir clients/typescript --filter @kayibal/fynd-client run test`

## Exclusive Liquidity (restricted)

A limited, opt-in service offered to specific deployments — **not** part of the normal routing path.
Ordinary deployments set no pool's `liquidity_scope`, every pool stays `LiquidityScope::PublicOnly`,
and nothing below applies. Treat it as out of scope unless a task names it.

Worker pools are partitioned by `LiquidityScope` (`fynd-core/src/worker_pool_router/`):

- `PublicOnly` (default) — public liquidity only. Its best candidate is the **committed amount out**,
  the reference output a quote must at least deliver
- `IncludeExclusive` — no filtering: routes through whatever the stream delivers, exclusive
  components included. A component is classified exclusive by `is_exclusive`
  (`fynd-core/src/feed/exclusivity.rs`), a fixed check for the `is_exclusive` static attribute on the
  component's Tycho data — not a per-deployment predicate. Its candidates may beat the public
  reference; the difference is **surplus**, tracked in `SurplusInfo` / `OrderQuote::surplus_amount()`
  and never serialized on the wire

Exclusive components only reach `MarketState` if the protocol's stream filter admits them. That is
opt-in per protocol via the `exclusive:` prefix on a `--protocols` entry (e.g.
`--protocols all_onchain,exclusive:ekubo_v3`), handled in
`fynd-core/src/feed/protocol_registry.rs`; `EXCLUSIVE_CAPABLE_PROTOCOLS` lists the protocols that
have such a variant (`ekubo_v3` only) and the prefix is rejected for any other. Stream admission is
independent of the routing scope below — opting in without any pool set to `IncludeExclusive` leaves
those pools indistinguishable from public liquidity everywhere.

Enabled per pool via `liquidity_scope = "include_exclusive"` in `worker_pools.toml`; a deployment
where every pool sets it fails the build (`SolverBuildError::NoPublicPool`) since no pool would be
left to establish the committed reference output. Encoding a leg that carries a committed amount is
protocol-specific and lives in `fynd-core/src/encoding/exclusive_swap.rs`, which needs
`EXCLUSIVE_SWAP_CONTROLLER_KEY`. See `fynd-core/CLAUDE.md` for the crate-level detail.

## Related Repositories

- **tycho-protocol-sdk**: Substreams modules that produce the on-chain data Tycho indexes
- **tycho-simulation**: Protocol-specific swap simulators (consumed via `tycho-simulation` crate)
- **tycho-execution**: Swap encoding and execution against Tycho router contracts
