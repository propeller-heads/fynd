//! Example: request quotes from a local Fynd instance.
//!
//! Runs a health check and two quote requests against the server at
//! `http://localhost:3000`. Only quote retrieval is exercised; no transactions
//! are formed.
//!
//! Run with the local dev environment:
//!
//! ```sh
//! ./scripts/run-example.sh quote
//! ```
//!
//! Or manually after starting `./scripts/dev-env.sh`:
//!
//! ```sh
//! cargo run --example quote -p fynd-client
//! ```

use bytes::Bytes;
use fynd_client::{FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams, QuoteStatus};
use num_bigint::BigUint;

const DEFAULT_FYND_URL: &str = "http://localhost:3000";

// Mainnet token addresses.
const WETH: &str = "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC: &str = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const USDT: &str = "dAC17F958D2ee523a2206206994597C13D831ec7";

// A well-known Ethereum address used as the sender/receiver.
const VITALIK: &str = "d8dA6BF26964aF9D7eEd9e03E53415D37aA96045";

fn addr(hex: &str) -> Bytes {
    Bytes::from(hex::decode(hex).expect("valid hex address"))
}

fn one_ether() -> BigUint {
    BigUint::from(1_000_000_000_000_000_000u64)
}

fn one_usdc() -> BigUint {
    // USDC has 6 decimals.
    BigUint::from(1_000_000u64)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fynd_url = std::env::var("FYND_URL").unwrap_or_else(|_| DEFAULT_FYND_URL.to_owned());

    // [doc:start quote-rust]
    let client = FyndClientBuilder::new(&fynd_url, &fynd_url)
        .build_quote_only()
        .map_err(|e| {
            format!(
                "{e}\n\nFynd not running at {fynd_url}. \
            Start the dev environment:\n  ./scripts/run-example.sh {}",
                env!("CARGO_BIN_NAME")
            )
        })?;

    // -----------------------------------------------------------------------
    // Health check
    // -----------------------------------------------------------------------
    let health = client.health().await?;
    println!("=== Health ===");
    println!("  healthy:            {}", health.healthy());
    println!("  last_update_ms:     {}", health.last_update_ms());
    println!("  num_solver_pools:   {}", health.num_solver_pools());
    println!("  derived_data_ready: {}", health.derived_data_ready());
    println!();

    // -----------------------------------------------------------------------
    // Quote 1: sell 1 WETH for USDC
    // -----------------------------------------------------------------------
    let quote = client
        .quote(QuoteParams::new(
            Order::new(addr(WETH), addr(USDC), one_ether(), OrderSide::Sell, addr(VITALIK), None),
            QuoteOptions::default(),
        ))
        .await?;

    println!("=== Quote: 1 WETH → USDC ===");
    println!("  order_id:      {}", quote.order_id());
    println!("  status:        {:?}", quote.status());
    println!("  amount_in:     {}", quote.amount_in());
    println!("  amount_out:    {}", quote.amount_out());
    println!("  gas_estimate:  {}", quote.gas_estimate());
    println!("  solve_time_ms: {}", quote.solve_time_ms());
    println!("  block:         #{} ({})", quote.block().number(), quote.block().hash());
    if let Some(route) = quote.route() {
        for (i, swap) in route.swaps().iter().enumerate() {
            println!(
                "  swap[{i}]: {} {} → {} (pool {})",
                swap.protocol(),
                swap.amount_in(),
                swap.amount_out(),
                swap.component_id(),
            );
        }
    }
    println!();
    // [doc:end quote-rust]

    assert_eq!(quote.status(), QuoteStatus::Success, "expected a successful WETH→USDC quote");
    assert!(quote.amount_out() > &BigUint::from(0u32), "amount_out must be non-zero");
    assert!(
        quote.block().hash().starts_with("0x"),
        "block hash must be a 0x-prefixed hex string, got: {}",
        quote.block().hash()
    );

    // -----------------------------------------------------------------------
    // Quote 2: sell 1 USDC for USDT (stablecoin pair, small amount)
    // -----------------------------------------------------------------------
    let quote2 = client
        .quote(QuoteParams::new(
            Order::new(addr(USDC), addr(USDT), one_usdc(), OrderSide::Sell, addr(VITALIK), None),
            QuoteOptions::default(),
        ))
        .await?;

    println!("=== Quote: 1 USDC → USDT ===");
    println!("  order_id:      {}", quote2.order_id());
    println!("  status:        {:?}", quote2.status());
    println!("  amount_in:     {}", quote2.amount_in());
    println!("  amount_out:    {}", quote2.amount_out());
    println!("  gas_estimate:  {}", quote2.gas_estimate());
    println!("  block:         #{} ({})", quote2.block().number(), quote2.block().hash());
    println!();

    assert_eq!(quote2.status(), QuoteStatus::Success, "expected a successful USDC→USDT quote");
    assert!(quote2.amount_out() > &BigUint::from(0u32), "amount_out must be non-zero");

    println!("All assertions passed.");
    Ok(())
}
