//! Custom algorithm example for fynd-core
//!
//! Demonstrates how to implement the [`Algorithm`] trait for a custom type and plug it
//! into [`FyndBuilder`] via [`FyndBuilder::with_algorithm`], without modifying
//! fynd-core itself.
//!
//! [`MyAlgorithm`] here is a thin wrapper around [`MostLiquidAlgorithm`]. In a real
//! integration you would replace the delegation in [`Algorithm::find_best_route`] with
//! your own routing logic.
//!
//! # Prerequisites
//!
//! ```bash
//! export TYCHO_API_KEY="your-api-key"  # Get from https://tycho.propellerheads.xyz
//! export RPC_URL="https://eth.llamarpc.com"
//! export TYCHO_URL="tycho-fynd-ethereum.propellerheads.xyz"  # Optional, defaults to tycho-beta
//! cargo run --package fynd-core --example custom_algorithm
//! ```

use std::{env, str::FromStr, time::Duration};

use fynd_core::{
    derived::SharedDerivedDataRef, feed::market_data::SharedMarketDataRef, types::RouteResult,
    Algorithm, AlgorithmConfig, AlgorithmError, ComputationRequirements, EncodingOptions,
    FyndBuilder, MostLiquidAlgorithm, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest,
};
use num_bigint::BigUint;
use tycho_simulation::{evm::tycho_models::Chain, tycho_core::Bytes};

// =============================================================================
// Custom algorithm implementation
// =============================================================================

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
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        // Delegate to the inner algorithm. Replace this with custom logic.
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

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tycho_url = env::var("TYCHO_URL")
        .unwrap_or_else(|_| "tycho-fynd-ethereum.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY env var not set");
    let rpc_url = env::var("RPC_URL").expect("RPC_URL env var not set");

    let solver = FyndBuilder::new(
        Chain::Ethereum,
        tycho_url,
        rpc_url,
        vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
        10.0,
    )
    .tycho_api_key(tycho_api_key)
    .with_algorithm("my_custom_algo", MyAlgorithm::new)
    .build()?;

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
