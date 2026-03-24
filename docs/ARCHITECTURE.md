---
icon: layer-group
---

# Architecture

## Overview

Fynd is a solver built on Tycho that finds optimal swap routes across DeFi protocols. It is organized as a multi-crate Rust workspace:

* **`fynd-core`** - Pure solving logic with no HTTP dependencies
* **`fynd-rpc`** - HTTP RPC server library
* **`fynd`** - CLI binary that runs the complete routing service

This modular architecture allows users to:

* Use just the routing algorithms (`fynd-core`) in their own applications
* Build custom HTTP servers with their own middleware (`fynd-rpc`)
* Run the complete solver as a standalone service (`fynd` binary)

## Design Decisions

* **Concurrency Model**: Hybrid async/threaded -- I/O on tokio, route finding on dedicated OS threads
* **Data Sharing**: `Arc<RwLock<>>` with write-preferring lock for SharedMarketData (single writer, many readers)
* **Path-Finding**: Pluggable `Algorithm` trait with associated graph types, allowing each algorithm to use its preferred graph representation
* **Graph Management**: `GraphManager` trait with incremental updates from market events; built-in implementation uses `petgraph::StableDiGraph`
* **Multi-Solver Competition**: Multiple worker pools with different configurations compete per request; WorkerPoolRouter selects the best result
* **Output Format**: Structured `Quote` objects (routes, amounts, gas estimates) with optional encoded transaction
* **Derived Data Pipeline**: Pre-computed spot prices, pool depths, and token gas prices fed to algorithms via a separate computation framework
* **Observability**: Prometheus metrics on port 9898, structured tracing, health endpoint

***

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           HTTP Layer (Actix Web)                            │
│                         Async I/O - Non-blocking                            │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │                           RouterApi                                  │   │
│  │    POST /v1/quote            GET /v1/health          GET /metrics    │   │
│  └───────────────────────────────┬──────────────────────────────────────┘   │
└──────────────────────────────────┼──────────────────────────────────────────┘
                                   │
                                   │ QuoteRequest
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            WorkerPoolRouter                                 │
│           Orchestrates multiple solver pools, selects best solution         │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │  • Fan-out: Send each order to ALL solver pools in parallel           │  │
│  │  • Timeout: Configurable deadline per request                         │  │
│  │  • Early return: Optional min_responses for fast path                 │  │
│  │  • Selection: Choose best solution by amount_out_net_gas              │  │
│  │  • Encoding: Optionally encode solution into on-chain transaction     │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │                             │
                    ▼                             ▼
┌─────────────────────────────────┐ ┌─────────────────────────────────┐
│  Worker Pool A                  │ │  Worker Pool B                  │
│  (most_liquid, 2 hops, fast)    │ │  (most_liquid, 3 hops, deep)    │
│  ┌───────────┐                  │ │  ┌───────────┐                  │
│  │ TaskQueue │ (per-pool)       │ │  │ TaskQueue │ (per-pool)       │
│  └─────┬─────┘                  │ │  └─────┬─────┘                  │
│        │                        │ │        │                        │
│  ┌─────┴─────┐  ┌───────────┐   │ │  ┌─────┴─────┐  ┌───────────┐   │
│  │  Worker 1 │  │  Worker N │   │ │  │  Worker 1 │  │  Worker M │   │
│  │(SolverWkr)│  │(SolverWkr)│   │ │  │(SolverWkr)│  │(SolverWkr)│   │
│  └───────────┘  └───────────┘   │ │  └───────────┘  └───────────┘   │
└─────────────────────────────────┘ └─────────────────────────────────┘
                    │                             │
                    └──────────────┬──────────────┘
                                   │ Reads shared data
                                   ▼
┌────────────────────────────────────────────────────────────────────────────────────┐
│                         SharedMarketData (Arc<RwLock<>>)                           │
│  ┌────────────────────────────────────────────────────────────────────────────┐    │
│  │  components: HashMap<ComponentId, ProtocolComponent>                       │    │
│  │  simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>>             │    │
│  │  tokens: HashMap<Address, Token>                                           │    │
│  │  gas_price: Option<BlockGasPrice>                                          │    │
│  │  protocol_sync_status: HashMap<String, SynchronizerState>                  │    │
│  │  last_updated: Option<BlockInfo>                                           │    │
│  └────────────────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────▲─────────────────────────────────────────────────┘
                                   │ WRITE lock
                                   │
┌──────────────────────────────────┴──────────────────────────────────────────┐
│                              TychoFeed                                      │
│                     Background task (single instance)                       │
│  ┌────────────────────────────────────────────────────────────────────┐     │
│  │  Tycho Stream ──► Update SharedMarketData ──► Broadcast Event      │     │
│  └────────────────────────────────────────────────────────────────────┘     │
│                                   │                                         │
│                                   ▼ broadcast::Sender<MarketEvent>          │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼              ▼
              ┌──────────┐   ┌──────────┐   ┌──────────┐
              │SolverWkr │   │SolverWkr │   │SolverWkr │
              │GraphMngr │   │GraphMngr │   │GraphMngr │
              │ updates  │   │ updates  │   │ updates  │
              │ graph    │   │ graph    │   │ graph    │
              │ on event │   │ on event │   │ on event │
              └──────────┘   └──────────┘   └──────────┘

┌──────────────────────────────────────────────────────────────────┐
│                     Derived Data Pipeline                        │
│                                                                  │
│  TychoFeed events ──► ComputationManager                         │
│                          │                                       │
│                          ├─ SpotPriceComputation                 │
│                          ├─ PoolDepthComputation (needs spots)   │
│                          ├─ TokenGasPriceComputation(needs spots)│
│                          │                                       │
│                          ▼                                       │
│                     DerivedData Store ──► broadcast events       │
│                                              │                   │
│                                    ┌─────────┼──────────┐        │
│                                    ▼         ▼          ▼        │
│                              Worker 1  Worker 2  Worker N        │
│                              (update edge weights on graph)      │
└──────────────────────────────────────────────────────────────────┘
```

***

## Components

### 1. API Layer (RouterApi)

**Crate:** `fynd-rpc` **Location:** `fynd-rpc/src/api/`

Actix Web HTTP handlers. Validates requests, delegates to WorkerPoolRouter, returns JSON responses.

**Endpoints:**

* `POST /v1/quote` -- Submit quote requests
* `GET /v1/health` -- Health check (data freshness, derived data readiness, pool count)
* `GET /metrics` -- Prometheus metrics (separate server, port 9898)

***

### 2. WorkerPoolRouter

**Crate:** `fynd-core` **Location:** `fynd-core/src/worker_pool_router/`

Orchestrates quote requests across multiple worker pools:

1. Fans out each order to all pools in parallel
2. Manages per-request timeouts with optional early return
3. Selects the best solution by `amount_out_net_gas`
4. Optionally encodes winning solutions into on-chain transactions (when `EncodingOptions` are provided)
5. Reports failures with error types and metrics

***

### 3. Worker Pool

**Crate:** `fynd-core` **Location:** `fynd-core/src/worker_pool/`

Manages dedicated OS threads for CPU-bound route finding. Each pool has:

* A name and algorithm assignment
* A bounded `TaskQueue` (via `async_channel`)
* N `SolverWorker` instances on separate threads

Pools can use either a built-in algorithm by name (e.g., `"most_liquid"`) or a custom `Algorithm` implementation via `WorkerPoolBuilder::with_algorithm`. Pools are configured via `worker_pools.toml` for built-in algorithms, or programmatically via the builder for custom algorithms. Multiple pools can use the same algorithm with different parameters (e.g., fast 2-hop vs deep 3-hop).

***

### 4. SolverWorker

**Crate:** `fynd-core` **Location:** `fynd-core/src/worker_pool/worker.rs`

Each worker:

1. Initializes a graph from market topology
2. Runs a prioritized `select!` loop: shutdown > market events > derived events > solve tasks
3. Maintains a `ReadinessTracker` for derived data requirements
4. Calls the algorithm's `find_best_route` with the local graph and shared market data

***

### 5. Algorithm Trait

**Crate:** `fynd-core` **Location:** `fynd-core/src/algorithm/`

Pluggable interface for route-finding algorithms:

* Specifies preferred graph type and graph manager via associated types
* Stateless: receives graph as parameter
* Declares derived data requirements (fresh vs stale)

**Built-in:** `MostLiquidAlgorithm` -- BFS path enumeration, depth-weighted scoring, ProtocolSim simulation, gas-adjusted ranking.

***

### 6. Encoding

**Crate:** `fynd-core` **Location:** `fynd-core/src/encoding/`

Encodes solved routes into on-chain transactions. When `EncodingOptions` are provided, delegates to `TychoEncoder` to produce ABI-encoded calldata for the appropriate router function (`singleSwap`, `sequentialSwap`, or their Permit2 variants).

***

### 7. Graph Module

**Crate:** `fynd-core` **Location:** `fynd-core/src/graph/`

Graph management infrastructure:

* `GraphManager` trait: initialize + incremental updates from events
* `PetgraphStableDiGraphManager`: Implementation using `petgraph::StableDiGraph`
* `EdgeWeightUpdaterWithDerived`: Updates edge weights from derived data (pool depths)
* `Path` type: Sequence of edges for route representation

***

### 8. SharedMarketData

**Crate:** `fynd-core` **Location:** `fynd-core/src/feed/market_data.rs`

Single source of truth for all market state. Contains components, simulation states, tokens, gas prices, sync status, and block info. Protected by `Arc<RwLock<>>` (write-preferring).

Provides `extract_subset()` for creating filtered snapshots that algorithms can use without holding the main lock.

***

### 9. TychoFeed

**Crate:** `fynd-core` **Location:** `fynd-core/src/feed/tycho_feed.rs`

Background task that connects to Tycho's WebSocket API, processes component/state updates, updates SharedMarketData, and broadcasts `MarketEvent`s. Applies TVL filtering with hysteresis (components are added at `min_tvl` and removed at `min_tvl / tvl_buffer_ratio`), token recency filtering (`traded_n_days_ago`), blacklisting, and token quality filtering.

***

### 10. Derived Data System

**Crate:** `fynd-core` **Location:** `fynd-core/src/derived/`

Pre-computes analytics from raw market data:

* `SpotPriceComputation`: Spot prices for all pool pairs
* `PoolDepthComputation`: Liquidity depth at configured slippage
* `TokenGasPriceComputation`: Token prices relative to gas token

Computations run in dependency order. Workers use `ReadinessTracker` to wait for required data before solving.

***

### 11. Gas Price Fetcher

**Crate:** `fynd-core` **Location:** `fynd-core/src/feed/gas.rs`

Background worker that fetches gas prices from the RPC node. Signaled by TychoFeed after each block update.

***

### 12. Builder

**Crate:** `fynd-rpc` **Location:** `fynd-rpc/src/builder.rs`

`FyndRPCBuilder` assembles the entire system: creates feed, worker pools, computation manager, worker pool router, and HTTP server. `Fynd` runs the system and handles graceful shutdown.

***

### 13. CLI Binary

**Crate:** `fynd` **Location:** `src/main.rs` and `src/cli.rs`

Command-line application that parses CLI arguments, sets up observability (tracing, metrics), and uses `FyndRPCBuilder` to run the complete routing service.

***

## Data Flow

### Quote Request Flow

```
Client POST /v1/quote
    │
    ▼
RouterApi (validate)
    │
    ▼
WorkerPoolRouter (fan-out to all pools)
    │
    ├──► Pool A Queue ──► Worker ──► Algorithm ──► Quote
    ├──► Pool B Queue ──► Worker ──► Algorithm ──► Quote
    ├──► Pool C Queue ──► Worker ──► Algorithm ──► Timeout
    │
    ▼
WorkerPoolRouter (select best by amount_out_net_gas)
    │
    ▼ (optional)
Encoder (encode solution into on-chain transaction)
    │
    ▼
JSON Response to Client
```

### Market Update Flow

```
Tycho WebSocket Stream
    │
    ▼
TychoFeed
    ├──► Write SharedMarketData (RwLock write)
    ├──► Broadcast MarketEvent
    │       ├──► Worker 1 GraphManager (update graph)
    │       ├──► Worker 2 GraphManager (update graph)
    │       └──► Worker N GraphManager (update graph)
    └──► Trigger Gas Price Fetcher
    └──► ComputationManager
            ├──► SpotPriceComputation
            ├──► PoolDepthComputation
            ├──► TokenGasPriceComputation
            └──► Broadcast DerivedDataEvent
                    └──► Workers (update edge weights + readiness)
```

***

## Threading Model

```
Actix/Tokio Runtime (async I/O)
├── HTTP Server handlers
├── TychoFeed (WebSocket client)
├── WorkerPoolRouter (async fan-out)
├── Gas Price Fetcher
└── Computation Manager

Worker Pool A (dedicated OS threads)
├── Thread 1: SolverWorker (local graph + single-thread tokio rt)
├── Thread 2: SolverWorker
└── Thread N: SolverWorker

Worker Pool B (dedicated OS threads)
├── Thread 1: SolverWorker
└── Thread M: SolverWorker
```

**Communication channels:**

* HTTP -> WorkerPoolRouter: direct call (same async runtime)
* WorkerPoolRouter -> Workers: `async_channel` per pool (bounded, backpressure)
* Workers -> WorkerPoolRouter: `oneshot` channel (single response)
* TychoFeed -> Workers: `broadcast` channel (MarketEvent)
* ComputationManager -> Workers: `broadcast` channel (DerivedDataEvent)
* All -> SharedMarketData: `Arc<RwLock<>>` (read-heavy)
