# fynd-client

Rust client for the [Fynd](https://fynd.xyz) DEX router.

Request swap quotes, build signable transaction payloads, and broadcast signed orders through
the Fynd RPC API — all from a single typed interface.

For documentation, guides, and API reference, visit **<https://docs.fynd.xyz/>**.

## Installation

```toml
[dependencies]
fynd-client = "0.35"
```

## Quick start

```rust,no_run
use fynd_client::{
    FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams,
};
use bytes::Bytes;
use num_bigint::BigUint;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Start a local Fynd instance first: https://docs.fynd.xyz/get-started/quickstart
    let client = FyndClientBuilder::new("http://localhost:3000", "http://localhost:8545")
        .build()
        .await?;

    let weth = Bytes::copy_from_slice(
        alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").as_slice(),
    );
    let usdc = Bytes::copy_from_slice(
        alloy::primitives::address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").as_slice(),
    );
    let sender = Bytes::copy_from_slice(
        alloy::primitives::address!("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045").as_slice(),
    );

    let quote = client
        .quote(QuoteParams::new(
            Order::new(
                weth,
                usdc,
                BigUint::from(1_000_000_000_000_000_000u64), // 1 WETH
                OrderSide::Sell,
                sender,
                None,
            ),
            QuoteOptions::default(),
        ))
        .await?;

    println!("amount out: {}", quote.amount_out());
    Ok(())
}
```

See the [`examples/`](examples/) directory for complete, runnable programs covering ERC-20
approvals, Permit2 transfers, and client fees.
