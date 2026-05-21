---
icon: code-branch
---

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

From [`fynd-core/examples/custom_algorithm.rs`](../../fynd-core/examples/custom_algorithm.rs):

```rust
/// A naive algorithm that finds a direct pool between two tokens.
///
/// This iterates through all edges in the routing graph, finds one that
/// connects `token_in` to `token_out`, simulates the swap, and returns
/// the first successful result. It only supports single-hop (direct) routes.
struct DirectPoolAlgorithm {
    timeout: Duration,
}

impl DirectPoolAlgorithm {
    fn new(_config: fynd_core::AlgorithmConfig) -> Self {
        Self { timeout: Duration::from_millis(100) }
    }
}

impl Algorithm for DirectPoolAlgorithm {
    // Reuse the built-in petgraph manager — it handles graph initialization and
    // market event updates automatically. We just need a simple graph with no
    // edge weights (unit `()` type).
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "direct_pool"
    }

    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: MarketData,
        label: Option<StateLabel>,
        _derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        let market = match label.as_ref() {
            Some(l) => market
                .read_labeled(l)
                .await
                .map_err(|e| AlgorithmError::Other(e.to_string()))?,
            None => market.read().await,
        };

        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::Other("gas price not available".to_string()))?
            .effective_gas_price()
            .clone();

        // Walk every edge looking for one that goes token_in → token_out.
        for edge_idx in graph.edge_indices() {
            let Some((src_idx, dst_idx)) = graph.edge_endpoints(edge_idx) else {
                continue;
            };
            let (src_addr, dst_addr) = (&graph[src_idx], &graph[dst_idx]);

            if src_addr != order.token_in() || dst_addr != order.token_out() {
                continue;
            }

            let component_id = &graph
                .edge_weight(edge_idx)
                .expect("edge exists")
                .component_id;

            // Look up component metadata and simulation state.
            let Some(component) = market.get_component(component_id) else {
                continue;
            };
            let Some(state) = market.get_simulation_state(component_id) else {
                continue;
            };
            let Some(token_in) = market.get_token(order.token_in()) else {
                continue;
            };
            let Some(token_out) = market.get_token(order.token_out()) else {
                continue;
            };

            // Simulate the swap.
            let result = match state.get_amount_out(order.amount().clone(), token_in, token_out) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let swap = Swap::new(
                component_id.clone(),
                component.protocol_system.clone(),
                token_in.address.clone(),
                token_out.address.clone(),
                order.amount().clone(),
                result.amount.clone(),
                result.gas,
                component.clone(),
                state.clone_box(),
            );

            let route = Route::new(vec![swap]);
            let net_amount_out = BigInt::from(result.amount);

            return Ok(RouteResult::new(route, net_amount_out, gas_price));
        }

        Err(AlgorithmError::Other(format!(
            "no direct pool from {:?} to {:?}",
            order.token_in(),
            order.token_out()
        )))
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::default()
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}
```

The example uses `PetgraphStableDiGraphManager<()>` so the worker infrastructure handles graph maintenance automatically. The algorithm walks graph edges to find a pool connecting the two tokens, simulates the swap, and constructs a `Swap` → `Route` → `RouteResult`.

## Wire it up

Pass your algorithm factory to `FyndBuilder::with_algorithm()` instead of the string-based `.algorithm()` method:

```rust
    let solver = FyndBuilder::new(
        Chain::Ethereum,
        tycho_url,
        rpc_url,
        vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
        10.0,
    )
    .tycho_api_key(tycho_api_key)
    .with_algorithm("direct_pool", DirectPoolAlgorithm::new)
    .build()?;
```

The factory closure receives an `AlgorithmConfig` (hop limits, timeout) and returns your algorithm instance. `FyndBuilder` handles all the infrastructure: Tycho feed, gas price fetcher, computation manager, and worker pool setup.

## Run the example

### Prerequisites

```bash
export TYCHO_API_KEY="your-api-key"
export RPC_URL="https://your-rpc-provider.com"
```

### Run

```bash
cargo run --package fynd-core --example custom_algorithm
```

The example connects to Tycho, loads market data, and solves a 1000 USDC → WBTC order using `DirectPoolAlgorithm`.

For the complete runnable example, see [`fynd-core/examples/custom_algorithm.rs`](../../fynd-core/examples/custom_algorithm.rs).
