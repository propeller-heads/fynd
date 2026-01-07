# Tycho Solver Architecture

## Overview

Tycho Solver is a high-performance solver built on Tycho for finding optimal swap routes across multiple DeFi protocols.

## Design Decisions

- **Concurrency Model**: RwLock upgrade (simpler, sufficient for initial load)
- **Path-Finding**: Flexible algorithm architecture, originally shipped with MostLiquid algorithm
- **Scope**: Production-ready (tracing, metrics, proper error types, token filtering)
- **Multi-Solver**: Shared data model with stateless algorithms
- **Output Format**: Structured Solution (not calldata) - encoding is separate concern
- **Worker Pool**: Dedicated thread pool for CPU-bound solving (separate from HTTP runtime)
- **Event Bus**: Broadcast channel for market updates to Solvers

---

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           HTTP Layer (Actix Web)                            │
│                         Async I/O - Non-blocking                            │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                           RouterApi                                  │   │
│  │    POST /solve              GET /health             GET /metrics    │   │
│  └───────────────────────────────┬──────────────────────────────────────┘   │
└──────────────────────────────────┼──────────────────────────────────────────┘
                                   │
                                   │ SolutionRequest
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            Task Queue (mpsc)                                │
│                     Bounded queue with backpressure                         │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                        Worker Pool (Dedicated Threads)                      │
│                       CPU-bound route computation                           │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐        │
│  │  Worker 1   │  │  Worker 2   │  │  Worker 3   │  │  Worker N   │        │
│  │  (Solver)   │  │  (Solver)   │  │  (Solver)   │  │  (Solver)   │        │
│  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘        │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │ Reads shared data
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         SharedMarketData (Arc<RwLock<>>)                    │
│  ┌────────────────────────────────────────────────────────────────────┐    │
│  │  pools: HashMap<PoolId, PoolData>                                   │    │
│  │    └── component: ProtocolComponent                                 │    │
│  │    └── state: Box<dyn ProtocolSim>    ← Heavy data, never cloned   │    │
│  │    └── tokens: Vec<Token>                                           │    │
│  │  tokens: HashMap<Address, Token>                                    │    │
│  │  route_graph: RouteGraph              ← Lightweight, clonable      │    │
│  │  gas_price: GasPrice                                                │    │
│  │  gas_constants: HashMap<ProtocolSystem, u64>                         │    │
│  └────────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────▲──────────────────────────────────────────┘
                                   │ WRITE lock
                                   │
┌──────────────────────────────────┴──────────────────────────────────────────┐
│                              TychoFeed                                   │
│                     Background task (single instance)                       │
│  ┌────────────────────────────────────────────────────────────────────┐    │
│  │  Tycho WebSocket ──► Update SharedMarketData ──► Broadcast Event   │    │
│  └────────────────────────────────────────────────────────────────────┘    │
│                                   │                                         │
│                                   ▼ broadcast::Sender<MarketEvent>          │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼              ▼
              ┌──────────┐   ┌──────────┐   ┌──────────┐
              │ Solver 1 │   │ Solver 2 │   │ Solver N │
              │ Updates  │   │ Updates  │   │ Updates  │
              │ local    │   │ local    │   │ local    │
              │ graph    │   │ graph    │   │ graph    │
              └──────────┘   └──────────┘   └──────────┘
```

---

## Components

### 1. RouterApi (HTTP Layer)

**File:** `src/api/`

**Responsibility:** Accept HTTP requests, validate input, enqueue tasks, return responses.

**Endpoints:**
- `POST /solve` - Submit solve requests
- `GET /health` - Health check
- `GET /info` - Service information

---

### 2. TaskQueue

**File:** `src/task_queue.rs`

**Responsibility:** Buffer solve requests, provide backpressure, distribute to workers.

```rust
pub struct TaskQueueHandle {
    sender: mpsc::Sender<SolveTask>,
}

impl TaskQueueHandle {
    pub async fn enqueue(&self, request: SolutionRequest) -> Result<Solution, SolveError>;
}
```

---

### 3. WorkerPool

**File:** `src/worker_pool.rs`

**Responsibility:** Manage dedicated compute threads, each owning a Solver instance.

```rust
pub struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
}
```

---

### 4. SharedMarketData

**File:** `src/market_data.rs`

**Responsibility:** Single source of truth for all market data. Updated by TychoFeed only.

```rust
pub struct SharedMarketData {
    pools: HashMap<PoolId, PoolData>,
    tokens: HashMap<Address, Token>,
    route_graph: RouteGraph,
    gas_price: GasPrice,
    gas_constants: HashMap<ProtocolSystem, u64>,
    last_updated: Instant,
}
```

---

### 5. RouteGraph (Lightweight, Clonable)

**File:** `src/route_graph.rs`

**Responsibility:** Graph topology only. Cloned by Solvers for local optimization.

```rust
#[derive(Clone)]
pub struct RouteGraph {
    adjacency: HashMap<Address, Vec<Edge>>,
    pool_tokens: HashMap<PoolId, Vec<Address>>,
}
```

---

### 6. TychoFeed

**File:** `src/tycho_feed.rs`

**Responsibility:** Connect to Tycho WebSocket, update SharedMarketData, broadcast events.

---

### 7. MarketEvent (Event Bus)

**File:** `src/events.rs`

**Responsibility:** Define events broadcast from Indexer to Solvers.

```rust
pub enum MarketEvent {
    PoolAdded { pool_id, tokens, protocol_type },
    PoolRemoved { pool_id },
    StateUpdated { pool_id },
    GasPriceUpdated { gas_price },
    Snapshot { pools, gas_price },
}
```

---

### 8. Solver

**File:** `src/solver.rs`

**Responsibility:** Own local RouteGraph, subscribe to events, execute algorithm.

```rust
pub struct Solver {
    local_graph: RouteGraph,
    algorithm: Box<dyn Algorithm>,
    market_data: SharedMarketDataRef,
    event_rx: broadcast::Receiver<MarketEvent>,
    config: SolverConfig,
}
```

---

### 9. Algorithm (Trait)

**File:** `src/algorithm/`

**Responsibility:** Define interface for route-finding algorithms.

```rust
pub trait Algorithm: Send + Sync {
    fn name(&self) -> &str;
    fn find_best_route(&self, graph: &RouteGraph, market: &SharedMarketData, order: &Order) -> Result<Route, AlgorithmError>;
    fn supports_exact_out(&self) -> bool { false }
    fn max_hops(&self) -> usize { 3 }
    fn timeout(&self) -> Duration { Duration::from_millis(50) }
}
```

---

## Data Flow

### Solve Request Flow

```
┌──────────┐    POST /solve   ┌───────────┐
│  Client  │ ───────────────▶ │ RouterApi │
└──────────┘                  └─────┬─────┘
                                    │
                              ┌─────▼─────┐
                              │ Validate  │
                              │ Request   │
                              └─────┬─────┘
                                    │
                              ┌─────▼─────┐     ┌───────────┐
                              │ TaskQueue │────▶│  oneshot  │
                              │ .enqueue()│     │  channel  │
                              └─────┬─────┘     └─────▲─────┘
                                    │                 │
                              ┌─────▼─────┐           │
                              │  Worker   │───────────┘
                              │  (picks)  │  response
                              └─────┬─────┘
                                    │
                              ┌─────▼─────┐
                              │  Solver   │
                              │  .solve() │
                              └─────┬─────┘
                                    │
              ┌─────────────────────┼─────────────────────┐
              │                     │                     │
              ▼                     ▼                     ▼
     ┌────────────────┐   ┌────────────────┐   ┌────────────────┐
     │ 1. Find paths  │   │ 2. For each    │   │ 3. Rank by     │
     │    in local    │   │    path, read  │   │    net output  │
     │    RouteGraph  │   │    states from │   │    (minus gas) │
     │                │   │    SharedData  │   │                │
     │                │   │    & simulate  │   │                │
     └────────────────┘   └────────────────┘   └────────────────┘
```

### Market Update Flow

```
┌───────────┐
│   Tycho   │
│ WebSocket │
└─────┬─────┘
      │ Update
      ▼
┌─────────────┐
│  TychoFeed  │
└────────┬────┘
         │
         ├────────────────────────────────────┐
         │                                    │
         ▼                                    ▼
┌─────────────────────────┐         ┌─────────────────────┐
│   SharedMarketData      │         │   Event Bus         │
│   (WRITE lock)          │         │   broadcast::send() │
└─────────────────────────┘         └──────────┬──────────┘
                                               │
                                    ┌──────────┼──────────┐
                                    ▼          ▼          ▼
                              ┌──────────┐┌──────────┐┌──────────┐
                              │ Solver 1 ││ Solver 2 ││ Solver N │
                              │ handle_  ││ handle_  ││ handle_  │
                              │ event()  ││ event()  ││ event()  │
                              └──────────┘└──────────┘└──────────┘
```

---

## Threading Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                     Actix Runtime (async, I/O bound)                    │
│                                                                         │
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐               │
│  │  HTTP Server  │  │   Indexer     │  │   Response    │               │
│  │   Handlers    │  │   Task        │  │   Collector   │               │
│  └───────────────┘  └───────────────┘  └───────────────┘               │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                  Worker Pool (dedicated OS threads, CPU bound)          │
│                                                                         │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐         │
│  │   Thread 1      │  │   Thread 2      │  │   Thread N      │         │
│  │   ┌─────────┐   │  │   ┌─────────┐   │  │   ┌─────────┐   │         │
│  │   │ Solver  │   │  │   │ Solver  │   │  │   │ Solver  │   │         │
│  │   │ (owns   │   │  │   │ (owns   │   │  │   │ (owns   │   │         │
│  │   │  local  │   │  │   │  local  │   │  │   │  local  │   │         │
│  │   │  graph) │   │  │   │  graph) │   │  │   │  graph) │   │         │
│  │   └─────────┘   │  │   └─────────┘   │  │   └─────────┘   │         │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘         │
└─────────────────────────────────────────────────────────────────────────┘

Communication:
  - HTTP → Workers: mpsc channel (SolveTask)
  - Workers → HTTP: oneshot channel (SolveResult)
  - Indexer → Workers: broadcast channel (MarketEvent)
  - All → SharedMarketData: Arc<RwLock<>> (read-heavy)
```

---

## File Structure

```
src/
├── lib.rs                    # Library root, re-exports
├── main.rs                   # Binary entry point
│
├── api/                      # HTTP Layer
│   ├── mod.rs
│   ├── handlers.rs           # Actix handlers
│   └── error.rs              # API error types
│
├── types/                    # Shared type definitions
│   ├── mod.rs
│   ├── api.rs                # Request/Response types
│   ├── solution.rs           # Solution, Route, Swap
│   ├── internal.rs           # SolveTask, SolveError
│   └── primitives.rs         # PoolId, Address, etc.
│
├── market_data.rs            # SharedMarketData
├── route_graph.rs            # RouteGraph (clonable)
├── events.rs                 # MarketEvent enum
│
├── task_queue.rs             # TaskQueue, TaskQueueHandle
├── worker_pool.rs            # WorkerPool
├── solver.rs                 # Solver
│
├── tycho_feed.rs                # TychoFeed
│
└── algorithm/                # Algorithm implementations
    ├── mod.rs                # Algorithm trait
    └── most_liquid.rs        # MostLiquidAlgorithm
```

---

## Success Criteria

1. **Performance**: 95% of solves < 50ms, 99% < 100ms
2. **Scalability**: Linear scaling with worker count
3. **Memory**: Single copy of ProtocolSim states (not duplicated per solver)
4. **Reliability**: No panics, graceful error handling
5. **Observability**: Prometheus metrics for latency, queue depth, cache hits
6. **Extensibility**: New algorithm = implement trait, register, done
