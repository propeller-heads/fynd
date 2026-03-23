# fynd-core

Pure solving logic for DEX routing. No HTTP dependencies — suitable for standalone use in custom
applications.

## Module Map

| Module | Description |
|---|---|
| `algorithm/` | `Algorithm` trait + built-in `MostLiquidAlgorithm`. Pluggable via associated graph types |
| `solver.rs` | `FyndBuilder` assembles the full pipeline (feed + gas + computations + pools + encoder + router). `Solver` runs it |
| `worker_pool/` | `WorkerPool` manages dedicated OS threads. `SolverWorker` runs a prioritized select loop (shutdown > market events > derived events > tasks). `TaskQueue` is `async_channel`-based |
| `worker_pool_router/` | `WorkerPoolRouter` fans out orders to all pools, selects best by `amount_out_net_gas`, optionally encodes |
| `feed/` | `TychoFeed` (WebSocket → SharedMarketData), `GasPriceFetcher`, `MarketEvent` broadcasting, `ProtocolRegistry` |
| `derived/` | `ComputationManager` runs `SpotPriceComputation`, `PoolDepthComputation`, `TokenGasPriceComputation` in dependency order. `ReadinessTracker` gates workers until data is fresh |
| `graph/` | `GraphManager` trait (initialize + incremental update), `PetgraphStableDiGraphManager`, `EdgeWeightUpdaterWithDerived`, `Path` type |
| `encoding/` | `Encoder` wraps `tycho-execution` to produce ABI-encoded calldata (singleSwap, sequentialSwap, Permit2 variants) |
| `types/` | Core types: `Order`, `Route`, `Swap`, `Quote`, `QuoteRequest`, `BlockInfo`, `EncodingOptions`, error types |

## Key Traits

### `Algorithm` (`algorithm/mod.rs`)
```rust
pub trait Algorithm: Send + Sync {
    type GraphType: Send + Sync;
    type GraphManager: GraphManager<Self::GraphType> + Default;
    fn name(&self) -> &str;
    async fn find_best_route(&self, graph: &Self::GraphType, market: SharedMarketDataRef, derived: Option<SharedDerivedDataRef>, order: &Order) -> Result<RouteResult, AlgorithmError>;
    fn computation_requirements(&self) -> ComputationRequirements;
    fn timeout(&self) -> Duration;
}
```

### `GraphManager` (`graph/mod.rs`)
```rust
pub trait GraphManager<G>: Send + Sync {
    fn initialize_graph(&mut self, components: &HashMap<ComponentId, Vec<Address>>);
    fn graph(&self) -> &G;
}
```

### `EdgeWeightUpdaterWithDerived` (`graph/mod.rs`)
```rust
pub trait EdgeWeightUpdaterWithDerived {
    fn update_edge_weights_with_derived(&mut self, market: &SharedMarketData, derived: &DerivedData) -> usize;
}
```

## Builder

**`FyndBuilder`** (`solver.rs`): Assembles feed + gas + computations + pools + encoder + router.
Returns a `Solver` that can `quote()` directly. For standalone (non-HTTP) use.

## Adding a Custom Algorithm

1. Implement `Algorithm` with your `GraphType` and `GraphManager`
2. Use `FyndBuilder::with_algorithm("name", factory)` or
   `WorkerPoolBuilder::with_algorithm("name", factory)`
3. No changes to fynd-core required

See `examples/custom_algorithm.rs` for a walkthrough.

## Data Flow

**Market updates** (every block):
1. `TychoFeed` writes new state into `SharedMarketData` (`Arc<RwLock<>>`)
2. Broadcasts `MarketEvent` → workers update local graph via `GraphManager`
3. Signals `GasPriceFetcher`
4. Triggers `ComputationManager` → `DerivedData` → workers update edge weights

**Solving** (`Solver::quote(request)`):
1. `WorkerPoolRouter` fans out to all pools in parallel
2. Each pool dispatches to a `SolverWorker` → `Algorithm::find_best_route` → `RouteResult`
3. Selects best by `amount_out_net_gas` → optional `Encoder` → `Quote`
