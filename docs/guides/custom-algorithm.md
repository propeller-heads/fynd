# Custom Algorithm

Fynd exposes an `Algorithm` trait that lets you plug in custom routing logic without modifying `fynd-core`. This guide walks through implementing the trait and wiring it into a worker pool.

## The `Algorithm` trait

The trait has four methods:

* `name()` — a string identifier used in config and logs
* `find_best_route()` — given a routing graph and an order, return the best route
* `computation_requirements()` — declares which derived data the algorithm needs (spot prices, depths, etc.)
* `timeout()` — per-order solve deadline

Your algorithm receives a read-only reference to the routing graph and shared market data. The worker infrastructure handles graph initialisation, event handling, and edge-weight updates.

## Implement the trait

From [`fynd-core/examples/custom_algorithm.rs`](https://github.com/propeller-heads/fynd/blob/main/fynd-core/examples/custom_algorithm.rs):

```rust
/// A custom algorithm that wraps [`MostLiquidAlgorithm`].
///
/// Replace the delegation in [`Algorithm::find_best_route`] with your own routing
/// logic to use a fully custom algorithm.
struct MyAlgorithm {
    inner: MostLiquidAlgorithm,
}

impl MyAlgorithm {
    fn new(config: AlgorithmConfig) -> Self {
        let inner =
            MostLiquidAlgorithm::with_config(config).expect("invalid algorithm configuration");
        Self { inner }
    }
}

impl Algorithm for MyAlgorithm {
    // Reuse the built-in graph type and manager so the worker infrastructure
    // (graph initialisation, event handling, edge weight updates) works unchanged.
    type GraphType = <MostLiquidAlgorithm as Algorithm>::GraphType;
    type GraphManager = <MostLiquidAlgorithm as Algorithm>::GraphManager;

    fn name(&self) -> &str {
        "my_custom_algo"
    }

    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: SharedMarketDataRef,
        derived: Option<SharedDerivedDataRef>,
        order: &fynd_core::Order,
    ) -> Result<RouteResult, AlgorithmError> {
        // Delegate to the inner algorithm.  Replace this with custom logic.
        self.inner
            .find_best_route(graph, market, derived, order)
            .await
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        self.inner.computation_requirements()
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }
}
```

Replace the delegation in `find_best_route` with your own routing logic. The `GraphType` and `GraphManager` associated types can also be replaced if you need a different graph structure.

## Wire it up

Pass a factory closure to `WorkerPoolBuilder::with_algorithm()` instead of the string-based `.algorithm()` method:

```rust
    let algorithm_config = AlgorithmConfig::new(1, 2, Duration::from_millis(5000), None)?;

    let (worker_pool, task_handle) = WorkerPoolBuilder::new()
        .name("custom-solver".to_string())
        .with_algorithm("my_custom_algo", MyAlgorithm::new)
        .algorithm_config(algorithm_config)
        .num_workers(2)
        .task_queue_capacity(100)
        .build(
            Arc::clone(&market_data),
            derived_data,
            pool_event_rx,
            derived_event_tx.subscribe(),
        )?;
```

The factory closure receives an `AlgorithmConfig` (hop limits, timeout) and returns your algorithm instance. The rest of the worker setup — graph loading, event routing, health reporting — is handled by the pool infrastructure.

## Run the example

### Prerequisites

```bash
export TYCHO_API_KEY="your-api-key"
export RPC_URL="https://eth.llamarpc.com"
export TYCHO_URL="tycho-beta.propellerheads.xyz"  # optional
```

### Run

```bash
cargo run --package fynd-core --example custom_algorithm
```

The example connects to Tycho, loads market data, and solves a 1000 USDC → WBTC order using `MyAlgorithm`.

For the complete runnable example, see [`fynd-core/examples/custom_algorithm.rs`](https://github.com/propeller-heads/fynd/blob/main/fynd-core/examples/custom_algorithm.rs).
