//! Example: sell 1 WETH for USDC using ERC-20 `transferFrom`.
//!
//! Approves the Fynd router to spend WETH (if needed), then executes
//! a real swap via the local Anvil fork started by the dev environment.
//!
//! Run with the local dev environment:
//!
//! ```sh
//! ./scripts/run-example.sh swap_erc20
//! ```
//!
//! Or manually after starting `./scripts/dev-env.sh`:
//!
//! ```sh
//! cargo run --example swap_erc20 -p fynd-client
//! ```

use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use bytes::Bytes;
use fynd_client::{
    ApprovalParams, EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide,
    QuoteOptions, QuoteParams, SignedApproval, SignedSwap, SigningHints,
};
use num_bigint::BigUint;

const DEFAULT_FYND_URL: &str = "http://localhost:3000";
const DEFAULT_RPC_URL: &str = "http://localhost:8545";
// Matches the key funded by scripts/dev-env.sh. Override with PRIVATE_KEY env var.
const DEV_PRIVATE_KEY: &str = "0x02d483ff876e4d1d55ddc829a22df2707bd2574ba18d0d870ef9c9edd3c0fe29";
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const SELL_AMOUNT: u128 = 1_000_000_000_000_000_000; // 1 WETH (18 decimals)
const SLIPPAGE: f64 = 0.005; // 0.5%

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fynd_url = std::env::var("FYND_URL").unwrap_or_else(|_| DEFAULT_FYND_URL.to_owned());
    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_owned());

    let private_key = std::env::var("PRIVATE_KEY").unwrap_or_else(|_| DEV_PRIVATE_KEY.to_owned());
    let signer: PrivateKeySigner = private_key.parse()?;
    let sender = signer.address();
    let sell_token: Address = WETH.parse()?;
    let buy_token: Address = USDC.parse()?;

    let client = FyndClientBuilder::new(&fynd_url, &rpc_url)
        .with_sender(sender)
        .build()
        .await
        .map_err(|e| {
            format!(
                "{e}\n\nFynd not running at {fynd_url}. \
            Start the dev environment:\n  ./scripts/run-example.sh {}",
                env!("CARGO_BIN_NAME")
            )
        })?;

    // Approve the router to spend WETH if the current allowance is insufficient.
    if let Some(approval_payload) = client
        .approval(
            &ApprovalParams::new(
                Bytes::copy_from_slice(sell_token.as_slice()),
                BigUint::from(SELL_AMOUNT),
                true,
            ),
            &SigningHints::default(),
        )
        .await?
    {
        println!("Approving router to spend WETH...");
        let sig = signer
            .sign_hash(&approval_payload.signing_hash())
            .await?;
        client
            .execute_approval(SignedApproval::assemble(approval_payload, sig))
            .await?
            .await?;
        println!("Approved.");
    }

    // Request a quote: sell 1 WETH for USDC.
    let quote = client
        .quote(QuoteParams::new(
            Order::new(
                Bytes::copy_from_slice(sell_token.as_slice()),
                Bytes::copy_from_slice(buy_token.as_slice()),
                BigUint::from(SELL_AMOUNT),
                OrderSide::Sell,
                Bytes::copy_from_slice(sender.as_slice()),
                None,
            ),
            QuoteOptions::default()
                .with_timeout_ms(5_000)
                .with_encoding_options(EncodingOptions::new(SLIPPAGE)),
        ))
        .await?;

    println!("amount_in:  {}", quote.amount_in());
    println!("amount_out: {}", quote.amount_out());

    // Sign and execute.
    let payload = client
        .swap_payload(quote, &SigningHints::default().with_simulate(true))
        .await?;
    let sig = signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let result = client
        .execute_swap(SignedSwap::assemble(payload, sig), &ExecutionOptions::default())
        .await?
        .await?;

    println!("settled:    {:?} USDC", result.settled_amount());
    println!("gas:        {}", result.gas_cost());
    Ok(())
}
