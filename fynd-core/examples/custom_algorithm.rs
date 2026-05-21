//! Custom algorithm example for fynd-core
//!
//! Demonstrates how to implement the [`Algorithm`] trait from scratch and plug it
//! into [`FyndBuilder`] via [`FyndBuilder::with_algorithm`], without modifying
//! fynd-core itself.
//!
//! [`DirectPoolAlgorithm`] is a naive algorithm that finds a single pool containing
//! both the input and output tokens, simulates the swap, and returns the result.
//! It only finds direct (1-hop) routes — no multi-hop routing.
//!
//! # Prerequisites
//!
//! ```bash
//! export TYCHO_API_KEY="your-api-key"  # Get from https://tycho.propellerheads.xyz
//! export RPC_URL="https://eth.llamarpc.com"
//! export TYCHO_URL="tycho-fynd-ethereum.propellerheads.xyz"  # Optional, defaults to chain-specific Fynd endpoint
//! cargo run --package fynd-core --example custom_algorithm
//! ```

use std::{env, str::FromStr, time::Duration};

use fynd_core::{
    derived::SharedDerivedDataRef,
    feed::market_data::{MarketData, StateLabel},
    graph::{PetgraphStableDiGraphManager, StableDiGraph},
    types::RouteResult,
    Algorithm, AlgorithmError, ComputationRequirements, EncodingOptions, FyndBuilder, Order,
    OrderQuote, OrderSide, QuoteOptions, QuoteRequest, Route, Swap,
};
use num_bigint::{BigInt, BigUint};
use tracing_subscriber::EnvFilter;
use tycho_simulation::{evm::tycho_models::Chain, tycho_core::Bytes};

// =============================================================================
// Custom algorithm implementation
// =============================================================================

// [doc:start custom-algo-impl]
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
// [doc:end custom-algo-impl]

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .compact()
        .init();

    let tycho_url = env::var("TYCHO_URL")
        .unwrap_or_else(|_| "tycho-fynd-ethereum.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY env var not set");
    let rpc_url = env::var("RPC_URL").expect("RPC_URL env var not set");

    // [doc:start custom-algo-wire]
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
    // [doc:end custom-algo-wire]

    println!("Waiting for market data and derived computations...");
    solver
        .wait_until_ready(Duration::from_secs(180))
        .await?;
    println!("Ready.\n");

    let order = Order::new(
        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?,
        Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599")?,
        BigUint::from(1_000_000_000u128), // 1000 USDC (6 decimals)
        OrderSide::Sell,
        "0x0000000000000000000000000000000000000000".parse()?,
    )
    .with_id("custom-algo-order".to_string());

    let options = QuoteOptions::default().with_encoding_options(EncodingOptions::new(0.01));
    let solution = solver
        .quote(QuoteRequest::new(vec![order], options))
        .await?;
    println!("Solved in {}ms\n", solution.solve_time_ms());

    print_route(&solution.orders()[0]);

    solver.shutdown();
    Ok(())
}

fn print_route(order_quote: &OrderQuote) {
    let Some(route) = order_quote.route() else {
        println!("No route found (status: {:?})", order_quote.status());
        return;
    };

    println!("Gas: {}\n", route.total_gas());
    println!("Route ({} hops):", route.swaps().len());

    for (i, swap) in route.swaps().iter().enumerate() {
        println!(
            "  {}. {} → {} amount_out={} ({})",
            i + 1,
            swap.token_in(),
            swap.token_out(),
            swap.amount_out(),
            swap.protocol()
        );
    }

    if let Some(tx) = order_quote.transaction() {
        let calldata: String = tx
            .data()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        println!("\nEncoded tx:\n  to:       {}\n  calldata: 0x{}", tx.to(), calldata);
    }
}
