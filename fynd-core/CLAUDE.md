# fynd-core

Pure solving logic for DEX routing. No HTTP dependencies — suitable for standalone use in custom
applications.

## Module Map

| Module                | Description                                                                                                                                                                        |
|-----------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `algorithm/`          | `Algorithm` trait + built-in `MostLiquidAlgorithm`, `BellmanFordAlgorithm`, `PathFrankWolfeAlgorithm`, `WaterFillAlgorithm`. Pluggable via associated graph types. `AlgorithmConfig` shared by built-ins |
| `solver.rs`           | `FyndBuilder` assembles the full pipeline (feed + gas + computations + pools + encoder + router). `Solver` runs it                                                                 |
| `worker_pool/`        | `WorkerPool` manages dedicated OS threads. `SolverWorker` runs a prioritized select loop (shutdown > market events > derived events > tasks). `TaskQueue` is `async_channel`-based |
| `worker_pool_router/` | `WorkerPoolRouter` fans out orders to all pools, ranks candidates by `amount_out_net_gas` descending; price guard (if enabled) validates in rank order; optionally encodes          |
| `feed/`               | `TychoFeed` (WebSocket → MarketState), `GasPriceFetcher`, `MarketEvent` broadcasting, `ProtocolRegistry`                                                                           |
| `derived/`            | `ComputationManager` runs `SpotPriceComputation`, `ComponentDepthComputation`, `TokenGasPriceComputation` in dependency order. `ReadinessTracker` gates workers until data is fresh     |
| `graph/`              | `pub` — `GraphManager` trait (initialize + incremental update), `PetgraphStableDiGraphManager`, `StableDiGraph` (re-exported), `EdgeWeightUpdaterWithDerived`, `Path` type           |
| `price_guard/`        | Price guard: external price validation for quotes. Sub-modules: `guard` (validation logic), `binance_ws` (Binance WebSocket price provider), `hyperliquid` (Hyperliquid oracle provider), `provider_registry`, `config`, `utils` |
| `replay.rs`           | `replay_route(&Route, &MarketState)` — re-execute an already-built route against a (possibly newer) market state, honoring split fractions and shared-pool depletion. Used by `hindsight` to measure quote-to-execution slippage |
| `encoding/`           | `Encoder` wraps `tycho-execution` to produce ABI-encoded calldata (singleSwap, sequentialSwap, Permit2 variants). Computes `FeeBreakdown` mirroring on-chain `FeeCalculator` logic. `RouterFees`/`SharedRouterFees` hold default + per-client fee rates; `RouterFeeFetcher` refreshes them from the FeeCalculator contract every 5 min |
| `types/`              | Core types: `Order`, `Route`, `Swap`, `Quote`, `QuoteRequest`, `BlockInfo`, `EncodingOptions`, `FeeBreakdown`, error types                                                         |

## Key Traits

### `Algorithm` (`algorithm/mod.rs`)

```rust
pub trait Algorithm: Send + Sync {
    type GraphType: Send + Sync;
    type GraphManager: GraphManager<Self::GraphType> + Default;
    fn name(&self) -> &str;
    async fn find_best_route(&self, graph: &Self::GraphType, market: MarketData, label: Option<StateLabel>, derived: Option<SharedDerivedDataRef>, order: &Order) -> Result<RouteResult, AlgorithmError>;
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
    fn update_edge_weights_with_derived(&mut self, market: MarketDataView<'_>, derived: &DerivedData) -> usize;
}
```

## Builder

**`FyndBuilder`** (`solver.rs`): Assembles feed + gas + computations + pools + encoder + router.
Returns a `Solver` that can `quote()` directly. For standalone (non-HTTP) use.

Price guard methods: `price_guard_enabled(bool)`, `register_price_provider(Box<dyn PriceProvider>)`,
`add_default_price_providers()` (registers Binance WS + Hyperliquid providers).

Additional builder methods: `partial_blocks(bool)` (enable flashblock/partial-block updates),
`with_pending_indexer(...)` (attach a pending-block indexer), `build_with_pending()` (build with
pending-block support). `Solver::subscribe_market_events()` returns a broadcast receiver for
`MarketEvent`s.

## Adding a Custom Algorithm

1. Implement `Algorithm` with your `GraphType` and `GraphManager`
2. Use `FyndBuilder::with_algorithm("name", factory)` or
   `WorkerPoolBuilder::with_algorithm("name", factory)`
3. No changes to fynd-core required

See `fynd-core/examples/custom_algorithm.rs` for a walkthrough.

## Integration Tests

`tests/integration/` replays a recorded market (`tests/fixtures/`, Git LFS) through the full
pipeline and asserts solution availability, quality vs baseline, derived-data metrics, and timing.
Run with `cargo nextest run -p fynd-core --features test-utils --test integration`. The
`test-utils` feature gates `Solver::from_recording` and the recording helpers; fixtures are
recorded with `tools/record-market`. See `tests/integration/README.md`.

## Data Flow

**Market updates** (every block):

1. `TychoFeed` writes new state into `MarketState` (via `MarketData`, `Arc<RwLock<>>`)
2. Broadcasts `MarketEvent` → workers update local graph via `GraphManager`
3. Signals `GasPriceFetcher`
4. Triggers `ComputationManager` → `DerivedData` → workers update edge weights

**Solving** (`Solver::quote(request)`):

1. `WorkerPoolRouter` fans out to all pools in parallel
2. Each pool dispatches to a `SolverWorker` → `Algorithm::find_best_route` → `RouteResult`
3. Selects best by `amount_out_net_gas` → optional `Encoder` → `Quote`

## Exclusive Liquidity (restricted)

A limited, opt-in service for specific deployments — **not** part of the normal routing path. With no
pool's `liquidity_scope` set (the default) every pool is `LiquidityScope::PublicOnly`, none of this
code runs, and the flows above are complete. Skip this section unless a task names it.

Exclusive components must first be admitted to the stream. `feed/protocol_registry.rs` parses each
`--protocols` entry into a `ProtocolSpec { system, exclusive }` (`Display` renders it back to the
entry form): the `exclusive:` prefix (e.g. `exclusive:ekubo_v3`) selects the protocol's
exclusive-inclusive filter — for Ekubo V3,
`ekubo_v3_extension_filter_with_signed_exclusive_swap` instead of the default
`ekubo_v3_extension_filter`, which drops SignedExclusiveSwap pools. The private
`EXCLUSIVE_CAPABLE_PROTOCOLS` is the single source of truth for which protocols accept the prefix;
applying it elsewhere is a `DataFeedError::Config`. The prefix is stripped before registration, so
Tycho only sees the bare system name.

`parse_protocols` also rejects a list naming one protocol both with and without the prefix. Registration
is keyed by system name (the stream builder and decoder both hold `HashMap`s), so such a list would
otherwise stream whichever variant happened to come last. Callers going through
`fynd_rpc::protocols::resolve_protocols` never hit this — it merges the variants first — but a
`Vec<String>` assembled by hand for `FyndBuilder::new` can, and gets an error rather than an
order-dependent stream.

Pools are partitioned by `LiquidityScope` (`worker_pool_router/`, re-exported at the crate root):

- `PublicOnly` (default) — public liquidity only. Its best candidate is the **committed amount out**,
  the reference output the quote must at least deliver.
- `IncludeExclusive` — no filtering: routes through whatever the stream delivers, exclusive
  components included if the deployment opted them into the stream.

A component is classified exclusive by `is_exclusive` (`feed/exclusivity.rs`) — a fixed check for the
`is_exclusive` static attribute on the component's Tycho data, applied generically to every ingested
component. There is no per-deployment policy to configure.

Isolation is per worker, not per state: `MarketState` is never duplicated. `PublicOnly` workers filter
exclusive components out of their local graph topology and incoming `MarketEvent`s (via
`remove_exclusive_components`/`scope_event`); `IncludeExclusive` workers ingest everything.

After public ranking, `combine_with_surplus` overlays any `IncludeExclusive`-scope candidate that
beats the committed amount and records the difference as `SurplusInfo`
(`OrderQuote::surplus_amount()`, `committed_amount_out()`, `Swap::committed_amount_out()`). All are
`#[serde(skip)]` — internal, not on the wire.

Enable by setting the scope per pool via `PoolConfig::with_liquidity_scope()` or
`liquidity_scope = "include_exclusive"` in `worker_pools.toml`. A deployment where every pool sets it
fails the build (`SolverBuildError::NoPublicPool`) — there would be no pool left to establish the
committed reference output.

Encoding a committed leg is protocol-specific: `encoding/exclusive_swap.rs` is Ekubo-only. It signs
an EIP-712 authorization with `EXCLUSIVE_SWAP_CONTROLLER_KEY` and packs it, plus a derived Q32 fee,
into the `SignedExclusiveSwap` extension's `user_data`. Encoding an exclusive leg without that env
var fails.
