# Tycho Solver Architecture

## Overview

Tycho Solver is a high-performance solver built on Tycho for finding optimal swap routes across multiple DeFi protocols.

## Design Decisions

- **Concurrency Model**: RwLock upgrade (simpler, sufficient for initial load)
- **Path-Finding**: Flexible algorithm architecture with generic graph types, originally shipped with MostLiquid algorithm
- **Graph Management**: Algorithms specify their graph type and graph manager via associated types, allowing different graph crates (petgraph, custom, etc.)
- **Scope**: Production-ready (tracing, metrics, proper error types, token filtering)
- **Multi-Solver**: Shared data model with stateless algorithms
- **Output Format**: Structured Solution (not calldata) - encoding is separate concern
- **Worker Pool**: Dedicated thread pool for CPU-bound solving (separate from HTTP runtime)
- **Event Bus**: Broadcast channel for market updates to Solvers
- **Market Topology**: Simple HashMap<ComponentId, Vec<Address>> representation, algorithms build their preferred graph structure

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
│  │(SolverWorker)│ │(SolverWorker)│ │(SolverWorker)│ │(SolverWorker)│        │
│  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘        │
└──────────────────────────────────┬──────────────────────────────────────────┘
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

**Responsibility:** Manage dedicated compute threads, each owning a SolverWorker instance.

```rust
pub struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
}
```

---

### 4. SharedMarketData

**File:** `src/feed/market_data.rs`

**Responsibility:** Single source of truth for all market data. Updated by TychoFeed only.

```rust
pub struct SharedMarketData {
    components: HashMap<ComponentId, ComponentData>,
    tokens: HashMap<Address, Token>,
    component_topology: HashMap<ComponentId, Vec<Address>>,  // Simple market graph representation
    gas_price: GasPrice,
    gas_constants: HashMap<ProtocolSystem, u64>,
    last_updated: Block,
}
```

The `component_topology` field stores a simple mapping from component IDs to their token addresses. Algorithms use their `GraphManager` to convert this into their preferred graph representation (e.g., petgraph::UnGraph).

---

### 5. Graph Module

**File:** `src/graph/`

**Responsibility:** Graph management infrastructure for algorithms.

**Components:**

- **GraphManager trait**: Defines interface for building and updating graphs from component topology
- **Edge & Path types**: Shared types for representing graph edges and paths
- **PetgraphGraphManager**: Implementation for petgraph::UnGraph

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

Algorithms specify their graph type and graph manager via associated types, allowing them to use different graph crates (petgraph, custom, etc.) and leverage built-in algorithms from graph libraries.

---

### 6. TychoFeed

**File:** `src/feed/tycho_feed.rs`

**Responsibility:** Connect to Tycho Stream, update SharedMarketData, broadcast events.

---

### 7. MarketEvent (Event Bus)

**File:** `src/feed/events.rs`

**Responsibility:** Define events broadcast from TychoFeed to SolverWorkers.

```rust
pub enum MarketEvent {
    ComponentAdded { component_id, tokens, protocol_system },
    ComponentRemoved { component_id },
    StateUpdated { component_id },
    GasPriceUpdated { gas_price },
    Snapshot { components, gas_price },
}
```

---

### 8. SolverWorker

**File:** `src/worker.rs`

**Responsibility:** Initialize graph on startup, subscribe to events, execute algorithm.

The solver worker is generic over the algorithm type and automatically infers the graph type and graph manager from the algorithm's associated types.

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
    config: WorkerConfig,
    initialized: bool,
}
```

The solver worker initializes the graph on startup by reading the component topology from SharedMarketData and calling `graph_manager.initialize_graph()`. The graph manager then maintains the graph internally and updates it based on market events. When solving, the solver worker gets the graph from the graph manager via `graph_manager.graph()`.

---

### 9. Algorithm (Trait)

**File:** `src/algorithm/`

**Responsibility:** Define interface for route-finding algorithms.

Algorithms specify their graph type and graph manager via associated types, allowing them to use different graph crates and leverage built-in algorithms.

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
    type GraphManager = PetgraphGraphManager;

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

- Algorithms are **stateless** - they receive graphs as parameters
- Each algorithm specifies its preferred graph type and graph manager via associated types
- The solver worker automatically creates the graph manager using `Default::default()`
- Graph managers handle converting `HashMap<ComponentId, Vec<Address>>` to the algorithm's graph type
- This allows algorithms to use different graph crates (petgraph, custom, etc.) and leverage built-in algorithms

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
                              │SolverWorker│──────────┘
                              │  .solve()  │  response
                              └─────┬─────┘
                                    │
              ┌─────────────────────┼─────────────────────┐
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
│  │  HTTP Server  │  │   TychoFeed   │  │   Response    │               │
│  │   Handlers    │  │   Task        │  │   Collector   │               │
│  └───────────────┘  └───────────────┘  └───────────────┘               │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                  Worker Pool (dedicated OS threads, CPU bound)          │
│                                                                         │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐         │
│  │   Thread 1      │  │   Thread 2      │  │   Thread N      │         │
│  │   ┌─────────┐   │  │   ┌─────────┐   │  │   ┌─────────┐   │         │
│  │   │SolverWorker│ │  │   │SolverWorker│ │  │   │SolverWorker│ │         │
│  │   │ (graph  │   │  │   │ (graph  │   │  │   │ (graph  │   │         │
│  │   │ manager │   │  │   │ manager │   │  │   │ manager │   │         │
│  │   │ maintains│   │  │   │ maintains│   │  │   │ maintains│   │         │
│  │   │ graph)  │   │  │   │ graph)  │   │  │   │ graph)  │   │         │
│  │   └─────────┘   │  │   └─────────┘   │  │   └─────────┘   │         │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘         │
└─────────────────────────────────────────────────────────────────────────┘

Communication:
  - HTTP → Workers: mpsc channel (SolveTask)
  - Workers → HTTP: oneshot channel (SolveResult)
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
├── types/                    # Shared type definitions
│   ├── mod.rs
│   ├── api.rs                # Request/Response types
│   ├── solution.rs           # Solution, Route, Swap
│   ├── internal.rs           # SolveTask, SolveError
│   └── primitives.rs         # ComponentId, Address, etc.
│
├── graph/                    # Graph management
│   ├── mod.rs                # GraphManager trait, Edge, Path
│   └── petgraph.rs           # PetgraphGraphManager
│
├── task_queue.rs             # TaskQueue, TaskQueueHandle
├── worker_pool.rs            # WorkerPool
├── worker.rs                 # SolverWorker
│
├── feed/                     # Market data feed
│   ├── mod.rs
│   ├── market_data.rs        # SharedMarketData
│   ├── events.rs             # MarketEvent enum
│   └── tycho_feed.rs         # TychoFeed
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
6. **Extensibility**: New algorithm = implement trait with associated types, specify graph type and manager, done
