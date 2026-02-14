# Fynd Architecture

## Overview

Fynd is a solver built on Tycho that finds optimal swap routes across DeFi protocols.

## Design Decisions

- **Concurrency Model**: RwLock upgrade (simpler, sufficient for initial load)
- **Path-Finding**: Flexible algorithm architecture with generic graph types. Ships with MostLiquid algorithm.
- **Graph Management**: Algorithms specify their graph type and graph manager via associated types, supporting different graph crates (petgraph, custom, etc.)
- **Scope**: Production-ready (tracing, metrics, proper error types, token filtering)
- **Multi-Solver**: Shared data model with stateless algorithms
- **Output Format**: Structured Solution (not calldata). Encoding is a separate concern.
- **Order Manager**: Fans out orders to multiple solver pools, manages timeouts, selects the best solution
- **Worker Pool**: Dedicated thread pool for CPU-bound solving (separate from HTTP runtime). Each pool runs one algorithm type.
- **Event Bus**: Broadcast channel for market updates to solvers
- **Market Topology**: Simple `HashMap<ComponentId, Vec<Address>>` representation. Algorithms build their preferred graph structure.

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
│                            OrderManager                                     │
│           Orchestrates multiple solver pools, selects best solution         │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │  • Fan-out: Send each order to ALL solver pools in parallel           │  │
│  │  • Timeout: Configurable deadline per request                         │  │
│  │  • Early return: Optional min_responses for fast path                 │  │
│  │  • Selection: Choose best solution by amount_out_net_gas              │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┴──────────────┐
                    │                             │
                    ▼                             ▼
┌─────────────────────────────────┐ ┌─────────────────────────────────┐
│  Worker Pool A (MostLiquid)     │ │  Worker Pool B (Future Algo)    │
│  ┌───────────┐                  │ │  ┌───────────┐                  │
│  │ TaskQueue │ (per-pool)       │ │  │ TaskQueue │ (per-pool)       │
│  └─────┬─────┘                  │ │  └─────┬─────┘                  │
│        │                        │ │        │                        │
│  ┌─────┴─────┐  ┌───────────┐   │ │  ┌─────┴─────┐  ┌───────────┐   │
│  │  Worker 1 │  │  Worker N │   │ │  │  Worker 1 │  │  Worker N │   │
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
│  │  component: HashMap<ComponentId, ComponentData>                            │    │
│  │    └── component: ProtocolComponent                                        │    │
│  │    └── state: Box<dyn ProtocolSim>    ← Heavy data, never cloned           │    │
│  │    └── tokens: Vec<Token>                                                  │    │
│  │  tokens: HashMap<Address, Token>                                           │    │
│  │  component_topology: HashMap<ComponentId, Vec<Address>>  ← Simple topology │    │
│  │  gas_price: GasPrice                                                       │    │
│  │  gas_constants: HashMap<ProtocolSystem, u64>                               │    │
│  └────────────────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────▲─────────────────────────────────────────────────┘
                                   │ WRITE lock
                                   │
┌──────────────────────────────────┴──────────────────────────────────────────┐
│                              TychoFeed                                      │
│                     Background task (single instance)                       │
│  ┌────────────────────────────────────────────────────────────────────┐    │
│  │  Tycho Stream ──► Update SharedMarketData ──► Broadcast Event      │    │
│  └────────────────────────────────────────────────────────────────────┘    │
│                                   │                                         │
│                                   ▼ broadcast::Sender<MarketEvent>          │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼              ▼
              ┌──────────┐   ┌──────────┐   ┌──────────┐
              │SolverWorker│ │SolverWorker│ │SolverWorker│
              │ GraphMgr │   │ GraphMgr │   │ GraphMgr │
              │ updates  │   │ updates  │   │ updates  │
              │ graph    │   │ graph    │   │ graph    │
              │ on event │   │ on event │   │ on event │
              └──────────┘   └──────────┘   └──────────┘
```

---

## Components

### 1. RouterApi (HTTP Layer)

**File:** `src/api/`

**Responsibility:** Accepts HTTP requests, validates input, delegates to OrderManager, returns responses.

**Endpoints:**

- `POST /solve` - Submit solve requests
- `GET /health` - Health check
- `GET /info` - Service information

---

### 2. OrderManager

**File:** `src/order_manager/`

**Responsibility:** Orchestrates multiple solver pools to find the best solution for each order.

```rust
pub struct OrderManager {
    solver_pools: Vec<SolverPoolHandle>,
    config: OrderManagerConfig,
}

pub struct OrderManagerConfig {
    pub default_timeout: Duration,  // Default: 100ms
    pub min_responses: usize,       // Default: 0 (wait for all)
}

pub struct SolverPoolHandle {
    pub name: String,       // Human-readable pool name (for logging/metrics)
    pub algorithm: String,  // Algorithm name
    pub queue: TaskQueueHandle,
}
```

**Key Features:**

1. **Fan-out**: Sends each order to all solver pools in parallel
2. **Timeout**: Configurable deadline per request (can be overridden via `SolutionOptions`)
3. **Early Return**: If `min_responses > 0`, returns as soon as N solvers respond
4. **Best Selection**: Chooses solution with highest `amount_out_net_gas`
5. **Error Tracking**: Captures all solver failures with error types (timeout, no route, etc.)

---

### 3. TaskQueue

**File:** `src/task_queue.rs`

**Responsibility:** Buffers solve requests, provides backpressure, distributes to workers within a pool.

Each WorkerPool has its own TaskQueue for independent backpressure per algorithm.

```rust
pub struct TaskQueueHandle {
    sender: mpsc::Sender<SolveTask>,
}

impl TaskQueueHandle {
    pub async fn enqueue(&self, request: SolutionRequest) -> Result<Solution, SolveError>;
}
```

---

### 4. WorkerPool

**File:** `src/worker_pool.rs`

**Responsibility:** Manages dedicated compute threads for a single algorithm type. Each pool has its own TaskQueue and SolverWorkers.

```rust
pub struct WorkerPool {
    name: String,       // Human-readable pool name (for logging/metrics)
    algorithm: String,  // Algorithm name (e.g., "most_liquid")
    workers: Vec<JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
}
```

Algorithms are registered in `src/algorithm/registry.rs`. To add a new algorithm:
1. Implement the `Algorithm` trait
2. Add a match arm in `spawn_workers()`
3. Add the name to `AVAILABLE_ALGORITHMS`

**Design Rationale (Queue per Pool):**

| Aspect | Benefit |
|--------|---------|
| Independent backpressure | Slow algorithm doesn't block fast ones |
| Independent scaling | Can have 8 workers for expensive algo, 2 for fast algo |
| Clean isolation | Algorithm bugs don't affect other pools |
| Easy extensibility | Add new algorithm = add new pool |

---

### 6. SharedMarketData

**File:** `src/feed/market_data.rs`

**Responsibility:** Single source of truth for all market data. Only TychoFeed writes to it.

```rust
pub struct SharedMarketData {
    /// All components indexed by their ID.
    components: HashMap<ComponentId, ProtocolComponent>,
    /// All states indexed by their component ID.
    simulation_states: HashMap<ComponentId, Box<dyn ProtocolSim>>,
    /// All tokens indexed by their address.
    tokens: HashMap<Address, Token>,
    /// Current gas price.
    gas_price: GasPrice,
    /// Protocol sync status indexed by their protocol system name.
    protocol_sync_status: HashMap<String, SynchronizerState>,
    /// Block info for the last update (only updated when protocols reported "Ready" status).
    /// None if no block has been processed yet.
    last_updated: Option<BlockInfo>,
}
```

`SharedMarketData::component_topology()` returns a mapping from component IDs to token addresses. Algorithms use their `GraphManager` to convert this into their preferred graph representation (e.g., `petgraph::UnGraph`).

---

### 7. Graph Module

**File:** `src/graph/`

**Responsibility:** Graph management infrastructure for algorithms.

- **GraphManager trait**: Interface for building and updating graphs from component topology
- **Edge & Path types**: Shared types for graph edges and paths
- **PetgraphStableDiGraphManager**: Implementation for `petgraph::stable_graph::StableDiGraph`

```rust
pub trait GraphManager<G>: Send + Sync {
    /// Initializes the graph from the market topology.
    /// Called once on solver startup.
    fn initialize_graph(&mut self, components: &HashMap<ComponentId, Vec<Address>>);

    /// Returns a reference to the managed graph.
    fn graph(&self) -> &G;

    /// Updates the graph based on a market event.
    fn handle_event(&mut self, event: &MarketEvent);
}
```

Algorithms specify their graph type and manager via associated types, so they can use different graph crates and their built-in algorithms.

---

### 8. TychoFeed

**File:** `src/feed/tycho_feed.rs`

**Responsibility:** Connects to Tycho Stream, updates SharedMarketData, broadcasts events.

---

### 9. MarketEvent (Event Bus)

**File:** `src/feed/events.rs`

**Responsibility:** Events broadcast from TychoFeed to SolverWorkers.

```rust
pub enum MarketEvent {
    /// Market was updated.
    MarketUpdated {
        added_components: HashMap<ComponentId, Vec<Address>>,
        removed_components: Vec<ComponentId>,
        updated_components: Vec<ComponentId>,
    }
}
```

---

### 5. SolverWorker

**File:** `src/worker.rs`

**Responsibility:** Initializes graph on startup, subscribes to events, executes algorithm.

The solver worker is generic over the algorithm type and infers the graph type and graph manager from the algorithm's associated types.

```rust
pub struct SolverWorker<A>
where
    A: Algorithm,
    A::GraphType: Send + Sync,
    A::GraphManager: GraphManager<A::GraphType>,
{
    algorithm: A,
    graph_manager: A::GraphManager,  // Maintains the graph internally
    market_data: SharedMarketDataRef,
    event_rx: broadcast::Receiver<MarketEvent>,
    initialized: bool,
}
```

On startup, the solver worker reads the component topology from SharedMarketData and calls `graph_manager.initialize_graph()`. The graph manager maintains the graph and updates it on market events. When solving, the worker reads the graph via `graph_manager.graph()`.

---

### 11. Algorithm (Trait)

**File:** `src/algorithm/`

**Responsibility:** Interface for route-finding algorithms.

Algorithms specify their graph type and manager via associated types, so they can use different graph crates and their built-in algorithms.

```rust
pub trait Algorithm: Send + Sync {
    /// The graph type this algorithm uses (e.g., petgraph::UnGraph<Address, Edge>)
    type GraphType: Send + Sync;

    /// The graph manager type for this algorithm
    type GraphManager: GraphManager<Self::GraphType> + Default;

    fn name(&self) -> &str;
    fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: &SharedMarketData,
        order: &Order,
    ) -> Result<Route, AlgorithmError>;
    fn supports_exact_out(&self) -> bool { false }
    fn max_hops(&self) -> usize { 3 }
    fn timeout(&self) -> Duration { Duration::from_millis(50) }
}
```

**Example Implementation:**

```rust
impl Algorithm for MostLiquidAlgorithm {
    type GraphType = UnGraph<Address, Edge>;
    type GraphManager = PetgraphStableDiGraphManager;

    fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: &SharedMarketData,
        order: &Order,
    ) -> Result<Route, AlgorithmError> {
        // Use petgraph's built-in algorithms here!
        // ...
    }
}
```

**Key Design Points:**

- Algorithms are **stateless**: they receive graphs as parameters
- Each algorithm specifies its graph type and manager via associated types
- The solver worker creates the graph manager using `Default::default()`
- Graph managers convert `HashMap<ComponentId, Vec<Address>>` to the algorithm's graph type

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
                              ┌─────▼─────┐
                              │  Order    │
                              │  Manager  │
                              └─────┬─────┘
                                    │
              ┌─────────────────────┼─────────────────────┐
              │ Fan-out to all      │                     │
              │ solver pools        │                     │
              ▼                     ▼                     ▼
     ┌────────────────┐   ┌────────────────┐   ┌────────────────┐
     │  Pool A Queue  │   │  Pool B Queue  │   │  Pool N Queue  │
     │  (MostLiquid)  │   │  (Future Algo) │   │  (Future Algo) │
     └───────┬────────┘   └───────┬────────┘   └───────┬────────┘
             │                    │                    │
             ▼                    ▼                    ▼
     ┌────────────────┐   ┌────────────────┐   ┌────────────────┐
     │    Workers     │   │    Workers     │   │    Workers     │
     │    (Solvers)   │   │    (Solvers)   │   │    (Solvers)   │
     └───────┬────────┘   └───────┬────────┘   └───────┬────────┘
             │                    │                    │
             └─────────────┬──────┴────────────────────┘
                           │ Collect responses
                           ▼
                    ┌──────────────┐
                    │ OrderManager │
                    │ select_best()│
                    │ by net_gas   │
                    └──────┬───────┘
                           │
                           ▼
                    ┌──────────────┐
                    │   Solution   │
                    │   Response   │
                    └──────────────┘
```

### Single Solver Flow (within a pool)

```
┌────────────────┐
│  SolveTask     │
│  from Queue    │
└───────┬────────┘
        │
        ▼
┌────────────────┐
│    Solver      │
│    .solve()    │
└───────┬────────┘
        │
        ├─────────────────────┬─────────────────────┐
        │                     │                     │
        ▼                     ▼                     ▼
┌────────────────┐   ┌────────────────┐   ┌────────────────┐
│ 1. Get graph   │   │ 2. Find paths  │   │ 3. Rank by     │
│    from        │   │    in graph,   │   │    net output  │
│    GraphManager│   │    read states │   │    (minus gas) │
│    (maintained │   │    from        │   │                │
│    internally) │   │    SharedData  │   │                │
│                │   │    & simulate  │   │                │
└────────────────┘   └────────────────┘   └────────────────┘
```

### Market Update Flow

```
┌───────────┐
│   Tycho   │
│   Stream  │
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
                              │SolverWorker││SolverWorker││SolverWorker│
                              │ GraphMgr ││ GraphMgr ││ GraphMgr │
                              │ updates  ││ updates  ││ updates  │
                              │ graph    ││ graph    ││ graph    │
                              │ on event ││ on event ││ on event │
                              └──────────┘└──────────┘└──────────┘
```

---

## Threading Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                     Actix Runtime (async, I/O bound)                    │
│                                                                         │
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐               │
│  │  HTTP Server  │  │   TychoFeed   │  │ OrderManager  │               │
│  │   Handlers    │  │   Task        │  │ (async fanout)│               │
│  └───────────────┘  └───────────────┘  └───────────────┘               │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│              Worker Pool A (dedicated OS threads, MostLiquid)           │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐         │
│  │   Thread 1      │  │   Thread 2      │  │   Thread N      │         │
│  │   ┌─────────┐   │  │   ┌─────────┐   │  │   ┌─────────┐   │         │
│  │   │SolverWkr│   │  │   │SolverWkr│   │  │   │SolverWkr│   │         │
│  │   │(graph   │   │  │   │(graph   │   │  │   │(graph   │   │         │
│  │   │ manager │   │  │   │ manager │   │  │   │ manager │   │         │
│  │   │maintains│   │  │   │maintains│   │  │   │maintains│   │         │
│  │   │ graph)  │   │  │   │ graph)  │   │  │   │ graph)  │   │         │
│  │   └─────────┘   │  │   └─────────┘   │  │   └─────────┘   │         │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘         │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│              Worker Pool B (dedicated OS threads, Future Algo)          │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐         │
│  │   Thread 1      │  │   Thread 2      │  │   Thread M      │         │
│  │   ┌─────────┐   │  │   ┌─────────┐   │  │   ┌─────────┐   │         │
│  │   │SolverWkr│   │  │   │SolverWkr│   │  │   │SolverWkr│   │         │
│  │   └─────────┘   │  │   └─────────┘   │  │   └─────────┘   │         │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘         │
└─────────────────────────────────────────────────────────────────────────┘

Communication:
  - HTTP → OrderManager: direct call (same async runtime)
  - OrderManager → Workers: async_channel per pool (SolveTask)
  - Workers → OrderManager: oneshot channel (SolveResult)
  - TychoFeed → Workers: broadcast channel (MarketEvent)
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
├── order_manager/            # Multi-solver orchestration
│   ├── mod.rs                # OrderManager, SolverPoolHandle
│   └── config.rs             # OrderManagerConfig
│
├── types/                    # Shared type definitions
│   ├── mod.rs
│   ├── api.rs                # Request/Response types
│   ├── solution.rs           # Solution, Route, Swap, Order
│   ├── internal.rs           # SolveTask, SolveError
│   └── primitives.rs         # ComponentId, Address, etc.
│
├── feed/                     # Market data feed
│   ├── mod.rs
│   ├── market_data.rs        # SharedMarketData
│   ├── events.rs             # MarketEvent enum
│   └── tycho_feed.rs         # TychoFeed (WebSocket client)
│
├── graph/                    # Graph management
│   ├── mod.rs                # GraphManager trait, Edge, Path
│   └── petgraph.rs           # PetgraphStableDiGraphManager
│
├── task_queue.rs             # TaskQueue, TaskQueueHandle
├── worker_pool.rs            # WorkerPool, WorkerPoolBuilder
├── worker.rs                 # SolverWorker
│
└── algorithm/                # Algorithm implementations
    ├── mod.rs                # Algorithm trait
    ├── registry.rs           # Algorithm registry for dynamic selection
    └── most_liquid.rs        # MostLiquidAlgorithm
```

---

## Success Criteria

1. **Performance**: 95% of solves < 50ms, 99% < 100ms
2. **Scalability**: Linear scaling with worker count
3. **Memory**: Single copy of ProtocolSim states (not duplicated per solver)
4. **Reliability**: No panics, graceful error handling
5. **Observability**: Prometheus metrics for latency, queue depth, cache hits
6. **Extensibility**: New algorithm = implement trait with associated types, specify graph type and manager, done
