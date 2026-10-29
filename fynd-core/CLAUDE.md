# fynd-core

Pure solving logic for DEX routing. No HTTP dependencies — suitable for standalone use in custom
applications.

## Module Map

| Module                | Description                                                                                                                                                                        |
|-----------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `algorithm/`          | `Algorithm` trait + built-in `MostLiquidAlgorithm`, `BellmanFordAlgorithm`, `PathFrankWolfeAlgorithm`, `WaterFillAlgorithm`. Pluggable via associated graph types. `AlgorithmConfig` shared by built-ins. `AlgorithmRegistry` (`registry.rs`) maps pool-config algorithm names to custom algorithms; built-in names live in `worker_pool/registry.rs` (`AlgorithmSpawner`, `AVAILABLE_ALGORITHMS`) |
| `solver.rs`           | `FyndBuilder` assembles the full pipeline (feed + gas + computations + pools + encoder + router). `Solver` runs it                                                                 |
| `worker_pool/`        | `WorkerPool` manages dedicated OS threads. `SolverWorker` runs a prioritized select loop (shutdown > market events > derived events > tasks). `TaskQueue` is `async_channel`-based. Two rules govern component admission to a worker's graph: the free function `should_drop_component` excludes exclusive components in a `PublicOnly` pool and the pool's `exclude_protocols`; `fallback::manager::PammManager` excludes pAMMs the market does not back. The algorithm's graph and fallback pricing also exclude `algorithm_exclude_protocols`. Post-processors still see those components and receive market events filtered only by the pool rule. The first holds for the worker's life; the pAMM rule expires, so `PammManager::apply_pamm_admission` re-decides it per market event, in both directions, on `propamm_admissions_total{outcome=admitted|withheld|evicted,pool}`. `price_impact` runs after route finding: it computes `price_impact_bps` from the route token map and each swap's reported spot price, supports linear and split routes, and reads no shared `MarketState`. On failure the worker omits the field, logs at debug, and increments `worker_pool_price_impact_calculations_total{pool,outcome}` |
| `worker_pool_router/` | `WorkerPoolRouter` allocates the worker pools that serve each order (`allocation`: `OrderClass` matched by `SolverPoolHandle::serves`, which also drops pools whose `SolverPoolHandle::supports_required_tokens` is false when the request requires tokens; a handle built by hand defaults to false; when that leaves an order with no pool, the whole solve fails with `RequiredTokensNotSupported`), fans out to those, drops candidates whose pAMM fallback misses `min_amount_out`, ranks the rest by `amount_out_net_gas` descending; price guard (if enabled) validates in rank order; optionally encodes |
| `feed/`               | `TychoFeed` (Tycho WebSocket plus non-Tycho sources → MarketState), `BookStream` (runs the off-chain book venues and applies their books), `book_venues/` (the `BookVenue` enum and one file per venue: its environment variables, support check, book settings and own connection; `open_book_feed` is the one place that picks a venue's feed), `GasPriceFetcher`, `MarketEvent` broadcasting, `protocol_registry` (free functions that parse and register `--protocols` entries), `metrics_sampler` (exports per-protocol market gauges on a timer). `component_filter` drops components from one worker's graph topology and incoming events; `exclusivity` classifies exclusive components. `TychoFeed` passes each Tycho update's sync status to `MarketState::update_sync_status`. It marks a protocol out of sync when Tycho reports it `Stale`, `Ended`, or `Delayed` with a block whose timestamp lags the last updated block's by more than `max_sync_lag` (`FyndBuilder::max_sync_lag`), and logs each transition. The worker keys its exclusion cache on `MarketState::exclusion_generation` and rejects a returned route through an unsynced pool. Fallback selection skips unsynced candidates. Components stay in `MarketState`, so a protocol routes again the block it recovers |
| `derived/`            | `ComputationManager` runs `SpotPriceComputation` and `TokenGasPriceComputation` in one stage, then `ComponentDepthComputation`, every block. Token gas pricing leaves pAMMs and RFQ venues out of its graph. `ReadinessTracker` gates workers until data is fresh. `FyndBuilder::with_computation` registers a caller's `DerivedComputation` (via `ComputationManager::register_boxed`) to run after the built-ins; a duplicate `ComputationId` fails the build, and readiness waits only on the built-ins |
| `graph/`              | `pub` — `GraphManager` trait (initialize + incremental update), `PetgraphStableDiGraphManager`, `StableDiGraph` (re-exported), `TopologyGraph` / `TopologyGraphManager` (`token_graph.rs`, the graph `MostLiquidAlgorithm` and `WaterFillAlgorithm` use), `EdgeWeightUpdaterWithDerived`, `Path` type |
| `fallback/`           | Picks the pool each `fallback:` leg falls back to and prices the route through it. A `fallback:` leg is a pAMM leg or a `fallback:rfq:` RFQ leg, and the module calls both "pAMM". Tycho's `TychoFallbackRouter` (or `MetricFallbackRouter` / `BebopFallbackRouter` / `HashflowFallbackRouter` for an RFQ leg) retries a failed leg on a fallback pool the encoder names, so the solver chooses it: `FallbackPoolIndex` indexes candidate pools by pair, admitting a component whose protocol system tycho-execution's `FallbackProtocol::from_protocol_system` maps to a protocol `supported_on` the component's chain (none on a chain with no router, so its pAMMs are all withheld), and never a hooked Uniswap V4 pool; `price_through_fallbacks` runs `select_fallback` on every pAMM leg, taking the candidate pool with the best simulated amount out that the request's route filter does not exclude, stamps it on the `Swap` as a `FallbackLeg`, and replays the route through those fallbacks in the same pass. `WorkerPoolRouter` drops the route candidate before ranking when that amount cannot clear `min_amount_out`, which keeps describing the pAMM quote and the user's slippage. Selection and replay share one `FallbackError`, whose `rejection()` gives the `RouteRejection` the worker reports. `PammManager` (`manager.rs`) owns one worker's pAMM state — the fallback pool index and where each pAMM stands: `withhold_from_graph` decides them all for a new graph, `apply_pamm_admission` re-decides them per event as candidate pools arrive and leave |
| `price_guard/`        | Price guard: external price validation for quotes. Sub-modules: `guard` (validation logic), `binance_ws` (Binance WebSocket price provider), `hyperliquid` (Hyperliquid oracle provider), `provider_registry`, `config`, `utils` |
| `simulation/`         | Encoded-quote simulation, switched on per deployment with `--enable-simulation`. `token_layout` — the storage slots a token keys its balances and allowances on, found by tracing the reads its own view functions make, so a non-standard layout (USDT, stETH shares, a proxy holding state elsewhere, a namespaced mapping such as Base's B20, Solady's seeded keys) can still be funded; a reflection token's balance is stored scaled (`BalanceEncoding::Scaled`, written through `TokenLayout::balance_word`); the tool crates use it too. `simulator` — runs the quote under `eth_simulateV1` with those overrides, caching one layout per token. `SimulationBlock` (crate root; `FyndBuilder::simulation_block`, CLI `--simulate-on-quote-block`) picks the latest block (default) or the quote's own block, which is retried for up to two block times while the node lacks it and otherwise yields `SimulationResult::BlockUnavailable`. The core `SimulationResult` (`#[non_exhaustive]`) is `Success`, `Reverted`, `Failure` or `BlockUnavailable`; on the wire the last three are `failure`. `SimulationPurpose` labels the simulation metrics (`purpose`). A quote requested with client fee params carries a placeholder client fee signature, because the client signs after quoting. The simulator gives the client fee receiver code that accepts any ERC-1271 signature. A revert logs the full `eth_simulateV1` payload, pinned to the block it ran in, under the opt-in `fynd::simulation_replay` debug target, so the call can be rerun off-line. `revert` — names the error a reverting call carries, replaying it under the call tracer when `eth_simulateV1` drops the payload. `deviation` — the quote-versus-simulation bps figure, shared by the log and the metric |
| `rpc.rs`              | private — `eth_call` and Tycho-address-to-alloy-address helpers used by `encoding::fee_fetcher` |
| `replay.rs`           | `replay_route(&Route, &MarketState)` — re-execute an already-built route against a (possibly newer) market state, honoring split fractions and shared-pool depletion. Exported for embedders that measure quote-to-execution slippage; in this crate `fallback/` and `algorithm/split_primitives.rs` use it. `simulate_swaps` (crate-private) runs a swap list on caller-chosen token and state lookups and returns `SimulatedRoute`, with the amount in, amount out and gas of each swap; the post-processors `RfqSwapReplacer` and `ExclusiveSwapReplacer` use it |
| `nexus/`       | `pub` — State that pods share through a `fynd-nexus` service. `NexusMessage` and `MessageBody` are the JSON messages the service serves; `NexusClient` connects a pod to the service; `rfq/` holds `RfqVenue`, `RfqAssembler` and `RfqPricesFeed`, which builds a venue's books from the prices the service serves |
| `encoding/`           | `Encoder` wraps `tycho-execution` to produce ABI-encoded calldata (singleSwap, sequentialSwap, Permit2 variants). Each order encodes on its own blocking task, `MAX_CONCURRENT_ENCODES` (32, the same cap `tycho-execution` applies to a batch) at a time, both hooks included, and an `encode_until` deadline (set only for retried candidates) covers the whole task, so one order's failure (an RFQ leg whose signed quote does not arrive, say) marks that order `QuoteStatus::EncodingFailed` and leaves every other order's calldata in place. A failure that belongs to one order never fails the call, as with `NoRouteFound` and `PriceCheckFailed`. Optional calldata watermark (`with_calldata_watermark`) appends attribution bytes the EVM ignores; a per-request `EncodingOptions::with_calldata_watermark` (serde-skipped, set by the serving layer) replaces it for that request. Two per-request hooks (`hooks.rs`, serde-skipped on `EncodingOptions`, re-exported at the crate root): `SolutionHook` (`with_solution_hook`) returns the tycho `Solution` to encode, at the end of `prepare_solution` after the fee-adjusted `min_amount_out` is set; it may change only the fields in `HOOK_MUTABLE_FIELDS` (`expected_amount_out`, `subsidy`), not below `min_amount_out`; the check diffs the serialized `Solution`s, so a field tycho adds later is covered, plus `Arc::ptr_eq` on each swap's unserialized `protocol_state` (RFQ encoders request their quote from it); any other changed field fails the quote. The quote's `amount_out` (with `amount_out_net_gas` moved by the same difference) follows the hooked `Solution`; the fee breakdown and per-swap route amounts do not. `CalldataHook` (`with_calldata_hook`) edits the router calldata in `attach_transaction`, before the watermark is appended, so the hook gets only the router call (the same bytes for every client), and simulation runs on the edited calldata. The hook gets a `&mut [u8]` so the length (and every offset) stays fixed. It may write into the swaps bytes (e.g. a subsidy signature): `FeeBreakdown::swaps_hash` is computed after the hook from the swaps in the final calldata (read back through `RouterCall::decode`). The calldata's values can differ from `solution`, `encoded` and `quote`, so a hook reads them from the calldata. `router_call.rs`: `RouterCall::decode` (public) reads back the router-call arguments the `ClientFee` signature covers, and the client fee params, for every layout; `RouterLayout` picks the layout from the function signature for both the encoder and the decoder, so the two cannot drift. A hook error fails only its quote (`EncodingFailed`), like an encoding error, so the retry step encodes the order's next non-RFQ candidate. Computes `FeeBreakdown` mirroring on-chain `FeeCalculator` logic. `RouterFees`/`SharedRouterFees` hold default + per-client fee rates; `RouterFeeFetcher` refreshes them from the FeeCalculator contract every 5 min. `DEFAULT_DEADLINE_WINDOW_SECS` (`exclusive_swap.rs`) is the validity window of the exclusive-swap signature. `with_jit_router` stores the chain's `FyndJITRouter`; `EncodingOptions::jit_router` (serde-skipped, set by the serving layer from the `x-jit-router` header) makes `attach_transaction` set it as `transaction.to`, calldata unchanged. `check_jit_router_encoding` fails with `EncodingUnavailable` when none is configured; a vault-funded swap keeps going to the router, which alone has vault entrypoints |
| `post_process/`       | `RoutePostProcessor` trait and `PostProcessorRegistry`. A pool config lists post-processor names in `post_processors`; `FyndBuilder::with_post_processors` supplies the registry, and `FyndBuilder` resolves the names, so an unknown name fails the build with `SolverBuildError::UnknownPostProcessor`. Each worker builds its own instances once, on its thread, and holds them in `PostProcessors` (`pipeline.rs`), which runs them. The worker calls `initialize` in `initialize_graph`, passes every filtered market event to `handle_event` after the graph manager, and in `quote` checks the algorithm's route first (route validation and the route filter, as without post-processors), then runs them in config order on it. `PostProcessRequest::drops_component` gives them the pool's drop rule, and `PostProcessRequest::exclusive_access` says whether they may add exclusive components: `Granted` only on a `public_only` pool for a caller with access, and the drop rule then keeps exclusive components. Post-processors receive the market events of exclusive components on a `public_only` pool too, though its graph drops them. A post-processor still running at the post-process deadline, 10 ms (`POST_PROCESS_DEADLINE_MARGIN`) before the task deadline, is cut off, and the worker keeps its input route, so that route still reaches the router in time. A post-processor reads the route by reference and returns `None` to keep it. A processed route that fails route validation, the request route filter, the unsynced-pool check, or the worker checks (it uses a component the pool drops, for a sell order has `net_amount_out` above its amount out, or has a `fallback:` leg that fallback pricing rejects) is logged at warn, counted in `worker_pool_post_processed_route_rejected_total{pool}`, and the quote uses the algorithm's route. `rfq_swap_replacer` (`rfq.rs`, crate-private `RfqSwapReplacer`) and `exclusive_swap_replacer` (`exclusive.rs`, crate-private `ExclusiveSwapReplacer`) are built in. `rfq_swap_replacer` replaces one swap or one segment of a sell order's route with a single RFQ swap when that raises the net amount out; only `PARTIAL_FILL_PROTOCOLS` (`rfq:bebop`, `rfq:hashflow`, `rfq:native`, also under `fallback:`) may replace swaps after the first, other RFQs only swaps starting at the sell token. `PostProcessorRegistry::resolve` finds them by name, and `with_post_processor` refuses those names. Both share the search in `replace.rs`: a `ReplacementComponentIndex` of the components a replacer may use, kept from market events, a `ReplacementLiquidity` of those components and the route tokens, and `ReplacementSolver`, which tries each target (one swap, or a segment between two tokens that every path passes through) and keeps the replacement with the highest net amount out. The `ReplacementFinder` trait gives each replacer its finder: `can_replace_target` says which components can replace a target, `find_best_replacement_route` builds and evaluates the replacement (by default, the target replaced by one swap), and a finder that gates or commits its replacements does so in `find_best_replacement_route`; a `CandidateRoute` carries the commitment, and `ReplacementSolver` puts the last route's commitment on the `RouteResult`. Each run records `worker_pool_post_processor_duration_seconds{pool,post_processor}` and `worker_pool_post_processor_outcomes_total{pool,post_processor,outcome}`, where `outcome` is `replaced`, `kept`, `timed_out` or `skipped` (the post-process deadline passed before it started). A replacement also records `worker_pool_post_processor_gain_bps{pool,post_processor}`, the gain in `net_amount_out` over the route the post-processor received, in bps (not recorded when that net is not positive; it counts a route that the worker checks later reject). `PostProcessors::run` returns a `ProcessedRoute` with the names of the post-processors that replaced the route; the worker puts them on the quote as `OrderQuote::post_processed_by` (empty when the checks reject the route; internal, not on the wire), and the router adds them as the `post_processed_by` label (names joined by `+`, or `none`) to `worker_router_best_quote_pool`, `worker_router_won_volume_gas_token` and `worker_router_unpriced_won_quotes_total` |
| `types/`              | Core types: `Order`, `Route`, `Swap`, `Quote`, `QuoteRequest`, `BlockInfo`, `EncodingOptions`, `FeeBreakdown`, error types. `RouteConstraints` is the liquidity a request excludes from a route (pools, protocol systems, tokens) and the tokens a route must pass through every one of, empty by default. `MarketState::resolve_route_filter` turns it into the `ResolvedRouteConstraints` a graph query and an algorithm read, with each protocol system replaced by the pools `MarketState::components_by_protocol` holds for it. `TopologyGraph::paths_between` drops token paths that miss any required token; an `Algorithm` opts in through `SUPPORTS_REQUIRED_TOKENS` (`most_liquid` and `water_fill` do), `WorkerPool::supports_required_tokens` reports it for the pool, and the worker fails the order on that pool with `RequiredTokensNotSupported` otherwise. `RouteConstraints::validate` rejects a token that is both required and excluded. Every `ResolvedRouteConstraints` also shares the market's unsynced pools. `ComponentTopology` is a component's tokens plus its swap directions; `MarketState` stores it with the component on upsert, and `upsert_components` reports which ids are new and which tracked ids changed their swap directions. See "RFQ aggregation" below.                                                         |

## Key Traits

### `Algorithm` (`algorithm/mod.rs`)

```rust
pub trait Algorithm: Send + Sync {
    type GraphType: Send + Sync;
    type GraphManager: GraphManager<Self::GraphType> + Default;
    fn name(&self) -> &str;
    async fn find_best_route(&self, request: SolveRequest<'_, Self::GraphType>) -> Result<RouteResult, AlgorithmError>;
    fn computation_requirements(&self) -> ComputationRequirements;
    fn timeout(&self) -> Duration;
}
```

### `GraphManager` (`graph/mod.rs`)

```rust
pub trait GraphManager<G>: Send + Sync
where
    G: Send + Sync,
{
    fn initialize_graph(&mut self, components: &FxHashMap<ComponentId, ComponentTopology>);
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
Simulation is deployment-gated with `simulation_enabled(bool)` and attached to the router with
`WorkerPoolRouter::with_simulator`; `simulation_block(SimulationBlock)` picks the block it runs on.
`with_computation(C)` adds a caller's `DerivedComputation` (see `derived/` above).

Additional builder methods: `partial_blocks(bool)` (enable flashblock/partial-block updates),
`with_pending_indexer(...)` (attach a pending-block indexer), `build_with_pending()` (build with
pending-block support), `build_with_pending_and_step_controller()` (the same, plus a
`BlockStepController` that holds each block until the caller releases it; feature `experimental`). `Solver::subscribe_market_events()` returns a broadcast receiver for
`MarketEvent`s.

## Adding a Custom Algorithm

1. Implement `Algorithm` with your `GraphType` and `GraphManager`
2. Register it and name it in the pool configuration, exactly as a built-in is named:
   ```rust
   let algorithms = AlgorithmRegistry::new().with_algorithm("my_algo", MyAlgorithm::new)?;
   FyndBuilder::new(..).with_algorithms(algorithms)
   ```
   ```toml
   [pools.mine]
   algorithm = "my_algo"
   ```
   `FyndRPCBuilder::with_algorithms` does the same for the HTTP server, and
   `Solver::from_recording_with` for a benchmark or profiler. A name that shipping code already
   uses is refused, so a registration cannot silently replace a built-in.
3. Report failures with the `AlgorithmError` constructors (`no_path`, `timeout`,
   `simulation_failed`, ...); the variants themselves cannot be built from outside the crate.
4. No changes to fynd-core required

See `fynd-core/examples/custom_algorithm.rs` for a walkthrough.

## Integration Tests

`tests/integration/` replays a recorded market (`tests/fixtures/`, Git LFS) through the full
pipeline and asserts solution availability, quality vs baseline, derived-data metrics, and timing.
Run with `cargo nextest run -p fynd-core --features test-utils --test integration`. The
`test-utils` feature gates `Solver::from_recording`, the recording helpers, and
`types::test_utils::OrderQuoteBuilder` (quotes for tests in other crates, encodable once given a
route, sender and receiver); fixtures are
recorded with `tools/record-market`. See `tests/integration/README.md`.

## Data Flow

**Market updates** (every block):

1. `TychoFeed` writes new state into `MarketState` (via `MarketData`, `Arc<RwLock<>>`)
2. Broadcasts `MarketEvent` → workers update local graph via `GraphManager`
3. Triggers `ComputationManager` → `DerivedData` → workers update edge weights

`GasPriceFetcher` runs independently on its own timer (`--gas-refresh-interval-secs`) and writes
the gas price into `MarketState`.

**Solving** (`Solver::quote(request)`):

0. `WorkerPoolRouter` allocates the pools serving each order — today an exclusive-access pool is
   allocated only to a request granted exclusive access; `QuoteOptions::with_worker_pools` further
   restricts allocation to a named subset
1. `WorkerPoolRouter` fans out to the allocated pools in parallel
2. Each pool dispatches to a `SolverWorker`, which resolves the request's `RouteConstraints` against the
   market and hands the result to `Algorithm::find_best_route` → `RouteResult`. An algorithm takes
   the request apart with the consuming `SolveRequest::into_parts` method (returns `SolveParts`); a route search reads its own
   `GraphQueryFilter` bounds and the request's `ResolvedRouteConstraints` through one `RouteSearch`
3. Selects best by `amount_out_net_gas` → optional `Encoder` → optional simulation → `Quote`.
   When an order's best candidate fails to encode, the next successful candidate without an RFQ
   leg is encoded instead, until one encodes, none remain, or the encoding retry budget (5ms by
   default, `WorkerPoolRouterConfig::with_encoding_retry_budget`) has passed since the first
   encode. A retry still encoding then is given up; the best candidates have no time limit.
   `encoding_retry_successes_total{best_route=rfq|other}` counts the orders a retry rescued. An RFQ
   leg fails to encode when the maker signs further below its price levels than the user's slippage,
   since the route is then likely to revert on-chain: `Encoder` sets the slippage as each swap's
   `max_signed_quote_shortfall_bps`, tycho-execution returns a `RecoverableError` reading "… bps
   below its price levels …", and `rfq_slippage_rejections_total{protocol,route_shape}` counts it.
   The check is per leg, exact for a `single_hop` route and stricter than needed for `multi_hop` and
   `split` ones.

Steps 0-2 are exposed as the public `WorkerPoolRouter::solve`, returning every order's ranked
candidates as `RankedQuotes`; step 3 splits into the public `encode_quotes`,
`WorkerPoolRouter::simulate_quotes` and `finalize_quote`, for embedders that need more than the
single best route per order. `simulate_quotes_for` is `simulate_quotes` with the caller's own
`SimulationPurpose` metric label. Between stages, `OrderQuote::set_amount_in` (clears the
transaction, fee breakdown and simulation result, so encode again), `set_amount_out` and
`retract(status)` adjust a quote, and `EncodingOptions::with_slippage` changes the slippage.

An embedder that composes those stages runs all four, in that order. `simulate_quotes` takes the
same `EncodingOptions` as `encode_quotes` and returns the quotes untouched when they do not ask
for simulation, so it is called unconditionally: a pipeline that tests the flag itself can drop
the stage by omission, and a request asking for simulation then comes back with no result and no
reason.

## Non-Tycho Liquidity Sources

Three forms of `--protocols` entry name streams other than Tycho's, each with its own endpoint. A fourth
source needs no entry: external liquidity a caller adds in code (below).

| Entry | Opened by | Stream |
|---|---|---|
| `rfq:<protocol>` | `BookStream::open` (`feed/book_stream.rs`) | Off-chain book venue publishing complete price-level snapshots through a latest-value feed |
| `fallback:rfq:<protocol>` | `BookStream::open` (`feed/book_stream.rs`) | The same book feed, labelling components `fallback:rfq:<protocol>` so they execute through that protocol's fallback router (`MetricFallbackRouter`, `BebopFallbackRouter`, `HashflowFallbackRouter`) |
| `pricelevelstream:<venue>` | `open_price_level_stream` (`feed/protocol_registry.rs`) | Titan pAMM price level WebSocket, an `impl Stream<Item = Update>` polled directly (it reconnects on its own, so there is no task to supervise) |

`register_exchanges` skips all three forms. In the normal `run` mode, `has_tycho_protocols` decides
whether to open a Tycho protocol stream at all. `run_with_pending` always builds the pending-capable
Tycho stream; when a step controller is requested, it explicitly rejects a configuration without a
Tycho-streamed protocol. `is_tycho_system` is the per-entry form, used by `register_exchanges` to
skip the non-Tycho entries and exported so `fynd_rpc::protocols` can leave them out of its Tycho
availability check. Both feed loops select over whichever sources they configured. Tycho and
price-level `Update`s go through `handle_tycho_message`; `BookUpdate`s go through
`handle_book_update`. Both non-Tycho stream groups are opened before `run_with_pending` answers its
`PendingFeedSetup` handshake, so a configuration error reaches the caller as an error rather than
as a handle to a feed that dies.

`FyndBuilder::with_external_liquidity(protocol_system, make_feed)` adds external liquidity: a feed of
the caller's own, which publishes `LiquiditySnapshot`s (`feed/external_liquidity.rs`). A snapshot holds
the `LiquidityComponent`s (a `ProtocolComponent` and an `Arc<dyn ProtocolSim>` state) the source serves
at that moment. `make_feed` gets an `ExternalLiquidityConfig` (chain, token universe, `min_tvl`) and
runs on every `BookStream::open`. `BookStream` merges the built-in venue feeds and the external feeds
(`external`) and turns both into the same `BookUpdate`s, so the rules below hold for both. Three checks
guard the market. `open` refuses a source named like any `--protocols` entry (or like the venue behind a
`rfq:` or `fallback:rfq:` entry), and two sources with one name. `BookStream` drops, with a warning, an
external component whose protocol system is not the source's name or that has a token outside the
token universe. `handle_book_update` drops, for every book feed, a component whose id the market holds
under another protocol system: it neither replaces nor removes it. The name's prefix decides the
treatment: `fallback:` makes pAMMs, `rfq:` puts the components under the RFQ replacer.

A book venue publishes states of the world, not incremental changes: the newest set replaces the
last one, and a pair missing from it is a pair the venue stopped serving. `BookStream` keeps the ids
each venue last published and derives the components to add and drop. A stale feed withdraws all of
its components and can restore them with a later publication. A feed that ends is removed for the
rest of the process; failed or exhausted feeds leave their components withdrawn, while a panic
stops the data feed so the orchestrator can restart it. Books belong to no block, so
`handle_book_update` writes directly to the base market state: it neither advances the market's
block label nor invalidates a simulation overlay built against the current block.

`open_book_feed` (`feed/book_venues/mod.rs`) has each venue module read its credentials from the
environment before building its feed, so a missing one is a `DataFeedError::Config` while opening
the stream rather than a feed that silently serves nothing: `BEBOP_KEY` for `rfq:bebop`, `HASHFLOW_USER`/`HASHFLOW_KEY` for
`rfq:hashflow`, `METRIC_API_KEY` for `rfq:metric`
(whose `METRIC_API_URL` stays optional, defaulting to Metric's public endpoint), `NATIVE_API_KEY`
for `rfq:native`, and `LIQUORICE_USER`/`LIQUORICE_KEY` for `rfq:liquorice`. `rfq:metric` is
also gated on `METRIC_CHAINS` (`feed/book_venues/metric.rs`), the chains tycho-execution deploys its executor to — elsewhere the
entry is rejected instead of pricing legs that cannot be encoded. On Ethereum, Metric can come
from either the book feed (`fallback:rfq:metric`, executing through `MetricFallbackRouter`) or the
price level stream (`pricelevelstream:metric`, executing through Titan's Metric router). Both
price the same pools, so `open_price_level_stream` rejects `pricelevelstream:metric` when the list
also names Metric's book feed, and the same inventory is never streamed twice.

A `fallback:rfq:` entry (`fallback:rfq:metric`, `fallback:rfq:bebop`, `fallback:rfq:hashflow`) is
rejected at registration unless tycho-execution's default `executor_addresses.json` lists that exact
protocol system for the chain. `fallback:rfq:metric` must also pass the `METRIC_CHAINS` gate. A list that names one RFQ protocol
both with and without `fallback:` is rejected too: both feeds would emit the same component ids under
different labels. The components are in the `fallback:` family, so the fallback module gives
their legs a fallback pool like a pAMM's, and a `fallback:` exclusion removes them.

Price level venues must be one of tycho-simulation's `default_served_pamms` — an unrecognised name
is a `DataFeedError::Config`, not a warning, because these entries are always hand-written. The
stream is Ethereum-only (`PRICE_LEVEL_STREAM_CHAIN` tracks upstream's venue set, which carries no
chain of its own). Its components arrive labelled `fallback:{venue}`, tycho-execution's
`FALLBACK_PREFIX` aliased by `fallback/`: every served venue executes through the
`TychoFallbackRouter`, and `matches_streamed_system` maps that label back to the
`pricelevelstream:` entry that asked for it. A venue served this way may also exist as a Tycho
protocol system (`vm:fermiswap` and `pricelevelstream:fermiswap` price the same maker inventory),
so drop the
Tycho one with an `exclude:` entry rather than streaming both. `EXCLUDE_PREFIX` and
`parse_exclusion` live here with the other entry prefixes; `fynd_rpc::protocols` applies them
after expanding `all_onchain`.

## Shared RFQ prices

With `--nexus-url`, `BookStream::open` makes a `NexusClient` and puts it in the
`BookVenueContext`. `open_book_feed` then builds the book assembler for `rfq:bebop`, `rfq:hashflow`
and `rfq:metric` (and their `fallback:` forms) from the same book settings the direct feed uses,
and hands it to `NexusClient::rfq_prices_feed` instead of opening the venue's
connection. Metric then needs no `METRIC_API_KEY`; Bebop and Hashflow still read their credentials,
which firm quotes use. Native and Liquorice always connect directly.

A `fynd-nexus` instance serves one chain, so the URL must be the instance of the pod's own chain.
The client does not check it.

`RfqPricesFeed` (`nexus/rfq/feed.rs`) is a tycho `SnapshotFeed`. Its private `PricePoller`
polls the service's `GET /v1/rfq/{venue}` every 500 ms with `If-None-Match`, and
`Publisher::publishing` publishes what it yields, so `BookStream` diffs its books and handles its
withdrawals like any direct feed. A `200` with a `snapshot` message becomes the books of the
venue's `RfqAssembler`, anchored at the snapshot's `as_of`; `withdrawn` becomes an empty set of
books; `304` and `204` publish nothing. A snapshot older than the venue's snapshot age, or with a
`seq` not above the last applied in the same `epoch`, is dropped. A failed poll is logged, counted
in `nexus_client_failed_polls_total` and retried after a wait that doubles from 500 ms to
8 s. The feed never gives up, and the venue's pools are withdrawn once it has applied no new
prices for the venue's snapshot age. The message format is in `nexus/mod.rs`.

## RFQ aggregation

`--aggregate-rfqs` makes `BookStream::open` stream Bebop and Hashflow (direct and `fallback:rfq:`) as
one component per venue instead of one per token pair. A route may take one quote from Bebop, and
one quote per market maker from Hashflow (`QuoteRule::OncePerMaker`). Metric, Native and Liquorice
always stream per pair. The venue lists the swap directions it quotes in its `swap_directions`
static attribute.

A component's graph edges follow its swap directions, as `types::swap_directions` reads them: the
attribute when present, otherwise every direction between its tokens. A direction the venue drops
loses its edge. Spot prices and depths exist for those directions only, and a dropped direction
loses its entries and failures.

`MarketState::upsert_components` compares the directions on every call. A re-sent component with the
same directions reaches `MarketEvent` as `updated_components`, like a state update. A new component,
or one with other directions, reaches `added_components`. The Tycho feed and the book feed both upsert every
component they receive, so a book venue that keeps its id and changes its directions is reported
as added.

## Exclusive Liquidity (restricted)

A limited, opt-in service for specific deployments — **not** part of the normal routing path. With no
pool's `liquidity_scope` set (the default) every pool is `LiquidityScope::PublicOnly`, none of this
code runs, and the flows above are complete. Skip this section unless a task names it.

Exclusive components must first be admitted to the stream. `feed/protocol_registry.rs` parses each
`--protocols` entry into a `ProtocolSpec { system, exclusive }` (`Display` renders it back to the
entry form): the `exclusive:` prefix (e.g. `exclusive:ekubo_v3`) selects the protocol's
exclusive-liquidity stream variant — for Ekubo V3,
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
`feed/component_filter.rs`, which every worker also uses for `exclude_protocols`);
`IncludeExclusive` workers ingest everything.

After public ranking, `combine_with_surplus` overlays any `IncludeExclusive`-scope candidate that
beats the committed amount and records the difference as `SurplusInfo`
(`OrderQuote::surplus_amount()`, `committed_amount_out()`, `Swap::committed_amount_out()`). All are
`#[serde(skip)]` — internal, not on the wire.

The one exclusive leg can sit at any hop of the route (`has_valid_exclusive_route`). When pools
follow it, `pin_commitment` converts the surplus into the leg's token at the average price of those
pools (`leg_route_output`): the route output the leg's output produces, divided by the leg's
output. The conversion runs no simulation. On split and merging routes the leg gets the share of
its token's route output that its own output makes up. The user gets at least the committed amount
when the pools after the leg give less per unit as their input grows, which holds for AMM pools.
After a hook pool, a market-maker quote with a minimum order size, or a fixed-rate pool (rounding),
the user can get slightly less; the router's `min_amount_out` bounds that loss by the request's
slippage.

The `exclusive_swap_replacer` post-processor (`post_process/exclusive.rs`) replaces a swap or a segment of a public route with an exclusive component, or splits one swap or a segment between its swaps and the exclusive component. It acts only on a `public_only` pool for a request with `ExclusiveAccess::Granted` (`PostProcessRequest::exclusive_access`; the worker passes `Denied` on an `include_exclusive` pool), so the order is solved once. It adds at most one exclusive leg, leaves a route that already has one unchanged, and keeps a candidate only when `exclusive_route_shape` is `Valid` and `matched_commitment` returns a commitment, with the route it received as the public reference. The commitment travels on the `RouteResult` (`with_commitment`: the committed amount out and the reference amount out). The worker pins it on the quote with `pin_route_commitment`, which records `exclusive_user_savings_amount` and calls `pin_commitment`, so the router ranks the quote like any public quote. The worker rejects a processed route that has an exclusive leg and no commitment on a `public_only` pool, and a commitment on a route with no exclusive leg, on a buy order, or above the route's amount out. A post-processor that runs after the replacer and builds a new `RouteResult` drops the commitment, so the replacer must be last: `check_post_processor_order` fails the build with `SolverBuildError::PostProcessorOrder` (`PostProcessorOrderError`) for a pool that lists it before another post-processor or twice.

The rules for an exclusive route (`ExclusiveRouteShape`, `exclusive_route_shape`), the route output of an exclusive leg (`leg_route_output`) and the commitment (`matched_commitment`, `pin_commitment`, `USER_IMPROVEMENT_SHARE_BPS`) live in `exclusive_route.rs`.

Two counters report the exclusive candidates `has_valid_exclusive_route` rejects:
`exclusive_route_multiple_legs_total` (more than one exclusive leg) and
`exclusive_route_does_not_reach_output_total` (the leg's output never reaches the output token, or
the swaps after it loop back to a token already on their path). A route with no exclusive leg is
not counted.

Two gauges report what the overlay is worth, in whole gas tokens: `exclusive_fee_amount` (LP fee
capture) and `exclusive_user_savings_amount` (user improvement over the public reference).
`to_gas_token_wei` converts without a price lookup — the quote states its gas cost both in wei
(`gas_estimate * gas_price`) and in output-token units (`amount_out - amount_out_net_gas`), so that
pair is the rate. An unpriced output token increments `exclusive_unpriced_output_total` instead.

Enable by setting the scope per pool via `PoolConfig::with_liquidity_scope()` or
`liquidity_scope = "include_exclusive"` in `worker_pools.toml`. A deployment where every pool sets it
fails the build (`SolverBuildError::NoPublicPool`) — there would be no pool left to establish the
committed reference output.

Encoding a committed leg is protocol-specific: `encoding/exclusive_swap.rs` is Ekubo-only. It signs
an EIP-712 authorization with `EXCLUSIVE_SWAP_CONTROLLER_KEY` and packs it, plus a derived Q32 fee,
into the `SignedExclusiveSwap` extension's `user_data`. Encoding an exclusive leg without that env
var fails.

The authorization names the Tycho router (the address `Encoder` resolves for the chain) as its
authorized locker, and the extension accepts no other caller. Authorizing every locker
(`Address::ZERO`) would let a third party that reads the signed bytes execute the swap from its own
contract and spend the nonce, which reverts the original transaction with `NonceAlreadyUsed`.

That locker is a router anyone may call, so it is not enough on its own: a copycat can wrap a router
call in its own contract and spend the payload at an amount nobody quoted. `pinned_min_balance_update`
narrows that by filling in the payload's `minBalanceUpdate` — two `i128` minimums the extension
checks against the pool's realized balance deltas, before applying its own fee. The pool's gain of
`token_in` sets the smallest input the swap may pay and its loss of `token_out` sets the largest
output the pool may pay out (`EXCLUSIVE_SWAP_OUTPUT_CAP_BPS`, default 5000 bps above the quote —
`from_env` reads it, and `with_output_cap_tolerance_bps` is the in-code path). The cap is wide
because a leg at the end of a route absorbs the drift of every hop before it, and a tight cap would
revert ordinary trades in thin pools. The floor scales by the request's own slippage
(`input_floor_factor_bps`), so a mid-route leg — which swaps whatever the previous hop delivered —
never reverts on drift the router's `min_amount_out` would have accepted. The cap does not use
slippage: that bounds how much less a taker will take, not how much more. The two fail in opposite directions: the cap
rejects an oversized swap, the floor rejects the reverse one. `swapParameters` travels outside the
signature, so these minimums are the only signed constraint on size and direction, and what remains
spendable is the band the two tolerances leave around the quote. Narrowing that band to the quoted
trade alone needs the payload to name the taker, and the extension has no field for anyone but the
locker.
