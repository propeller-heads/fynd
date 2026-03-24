//! Solve a swap order using fynd-core
//!
//! Demonstrates setting up the solver via [`FyndBuilder`] and solving a swap order.
//! Connects to Tycho's live feed for real market data.
//!
//! # Prerequisites
//!
//! ```bash
//! export TYCHO_API_KEY="your-api-key"  # Get from https://tycho.propellerheads.xyz
//! export RPC_URL="https://eth.llamarpc.com"
//! export TYCHO_URL="tycho-fynd-ethereum.propellerheads.xyz"  # Optional, defaults to chain-specific Fynd endpoint
//! cargo run --package fynd-core --example solve_order
//! ```
use std::{env, str::FromStr, time::Duration};

use fynd_core::{
    EncodingOptions, FyndBuilder, Order, OrderQuote, OrderSide, QuoteOptions, QuoteRequest,
};
use num_bigint::BigUint;
use tycho_simulation::{evm::tycho_models::Chain, tycho_core::Bytes};

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
    .algorithm("most_liquid")
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
    .with_id("example-order".to_string());

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
