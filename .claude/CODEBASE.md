<!-- docs-synced-at: 55b446762d25462cf184f9ea653050ae0a0b8dbd -->
# Fynd Codebase Guide

High-performance DeFi route-finding engine built on Tycho. Finds optimal swap routes across
multiple DeFi protocols in real-time.

## What is Fynd

Fynd is a solver that indexes live DEX liquidity via Tycho's streaming API, maintains an in-memory
graph of token pairs and pools, and runs pluggable routing algorithms on dedicated OS threads to
find optimal swap paths. It exposes an HTTP RPC for quote requests and returns the best
gas-aware solution with optional on-chain transaction encoding.

Key properties:
- **Multi-protocol**: Routes through any on-chain protocol supported by Tycho, plus RFQ protocols
- **Real-time**: Tycho Stream keeps all pool states synchronized every block
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

## Architecture Overview

See `docs/ARCHITECTURE.md` for the full architecture diagram and detailed component descriptions.

### Core Components

1. **RouterApi** (`fynd-rpc/src/api/`) — Actix Web HTTP handlers: `POST /v1/quote`, `GET /v1/health`, `GET /v1/info`
2. **WorkerPoolRouter** (`fynd-core/src/worker_pool_router/`) — Fans out orders to all pools, selects best by `amount_out_net_gas`
3. **WorkerPool** (`fynd-core/src/worker_pool/`) — N `SolverWorker` instances on dedicated OS threads per pool
4. **Algorithm trait** (`fynd-core/src/algorithm/`) — Pluggable route-finding; built-in: `MostLiquidAlgorithm`, `BellmanFordAlgorithm`
5. **SharedMarketData** (`fynd-core/src/feed/market_data.rs`) — `Arc<RwLock<>>` of all pool/token/gas state
6. **TychoFeed** (`fynd-core/src/feed/tycho_feed.rs`) — Background task: Tycho WebSocket → SharedMarketData → broadcast events
7. **Derived Data** (`fynd-core/src/derived/`) — Pre-computed spot prices, pool depths, token gas prices
8. **Encoding** (`fynd-core/src/encoding/`) — Encodes solved routes into on-chain transactions via `TychoEncoder`
9. **Graph** (`fynd-core/src/graph/`) — `GraphManager` trait + `PetgraphStableDiGraphManager` implementation

### Data Flow

**Market update path** (continuous, every block):
1. `TychoFeed` receives state updates from Tycho WebSocket
2. Writes new component/token/state data into `SharedMarketData` (write lock)
3. Broadcasts `MarketEvent` → each `SolverWorker` updates its local graph via `GraphManager`
4. Signals `GasPriceFetcher` → fetches gas price from RPC node → writes to `SharedMarketData`
5. Triggers `ComputationManager` → runs spot prices → pool depths → token gas prices (in dependency order) → broadcasts `DerivedDataEvent` → workers update edge weights

**Quote request path** (`POST /v1/quote`):
1. `RouterApi` validates the request
2. `WorkerPoolRouter` fans out each order to all worker pools in parallel
3. Each pool's `TaskQueue` dispatches to a `SolverWorker` on a dedicated OS thread
4. Worker calls `Algorithm::find_best_route` with its local graph + shared market/derived data
5. `WorkerPoolRouter` collects results, selects best by `amount_out_net_gas`
6. If `EncodingOptions` provided, `Encoder` produces ABI-encoded calldata
7. Returns `Quote` response

### Threading Model

- **Actix/Tokio runtime** (async I/O): HTTP server, TychoFeed, WorkerPoolRouter, gas fetcher, ComputationManager
- **Worker pools** (dedicated OS threads): Each `SolverWorker` has a local graph and single-thread tokio runtime
- **Communication**: `async_channel` (pool queues), `oneshot` (responses), `broadcast` (events), `Arc<RwLock<>>` (shared data)

## Configuration

### Environment Variables

| Variable | Purpose |
|---|---|
| `TYCHO_API_KEY` | Tycho API key (optional) |
| `RPC_URL` | Ethereum RPC endpoint (default: `https://eth.llamarpc.com`) |
| `TYCHO_URL` | Tycho endpoint (chain-specific default) |
| `HTTP_HOST` | HTTP bind address (default: `0.0.0.0`) |
| `HTTP_PORT` | API port (default: `3000`) |
| `WORKER_POOLS_CONFIG` | Worker pools config file (default: `worker_pools.toml`) |
| `BLOCKLIST_CONFIG` | Blocklist config file (default: `blocklist.toml`) |
| `RUST_LOG` | Tracing filter (e.g. `info,fynd=debug`) |

### CLI Commands

| Command | Purpose |
|---|---|
| `serve` | Run the solver: Tycho feed + HTTP RPC server |
| `openapi` | Print the OpenAPI spec JSON to stdout |

### Config Files

| File | Purpose |
|---|---|
| `worker_pools.toml` | Worker pool definitions: algorithm, num_workers, hop limits, timeout. Optional — binary falls back to embedded defaults if not found |
| `blocklist.toml` | Pool IDs to exclude from routing. Optional — binary falls back to embedded defaults if not found |

## Testing

- `cargo nextest run --workspace --all-targets --all-features` — full test suite
- `cargo +nightly clippy --workspace --all-targets --all-features` — lint
- `cargo +nightly fmt --all --check` — format check
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked --package fynd-core --package fynd-rpc-types --package fynd-rpc --package fynd-client` — doc build (broken links, missing docs)
- OpenAPI drift: `cargo run -- openapi | jq 'del(.info.version)'` vs `clients/openapi.json`
- TypeScript: `pnpm --dir clients/typescript install && pnpm --dir clients/typescript --filter @kayibal/fynd-client run test`

## Related Repositories

- **tycho-protocol-sdk**: Substreams modules that produce the on-chain data Tycho indexes
- **tycho-simulation**: Protocol-specific swap simulators (consumed via `tycho-simulation` crate)
- **tycho-execution**: Swap encoding and execution against Tycho router contracts
