//! Example: sell 1 WETH for USDC with a client fee.
//!
//! Demonstrates how to attach `ClientFeeParams` to a quote request so the
//! Tycho Router charges a client fee on the swap output.
//!
//! Two keys are used: the dev key as the sender, and a random ephemeral key
//! as the fee receiver (in production this is the integrator's key).
//!
//! The EIP-712 `ClientFee` type hash includes swap-specific fields (`amountIn`,
//! `tokenIn`, `tokenOut`, `minAmountOut`, `receiver`, `bytes swaps`) that are
//! only known after the server has encoded the transaction. The server returns
//! `swaps_hash` in the fee breakdown and `signature_offset` in the transaction so the client can
//! sign and patch the calldata locally:
//!
//! 1. Request a quote with unsigned client fee params (empty signature).
//! 2. Sign the full 10-field EIP-712 hash using `swaps_hash` from the response.
//! 3. Patch the signature into the calldata via `quote.with_client_fee_signature()`.
//! 4. Execute.
//!
//! ## Run with Anvil (mocked accounts)
//!
//! Requires `TYCHO_API_KEY` and `TYCHO_URL` env vars to be set:
//!
//! ```sh
//! ./scripts/run-example.sh swap_client_fee
//! ```
//!
//! ## Run with a real wallet
//!
//! Requires a Fynd server running in the background. Set `PRIVATE_KEY` to
//! a funded wallet's private key:
//!
//! ```sh
//! PRIVATE_KEY=0x... cargo run --example swap_client_fee -p fynd-client
//! ```

use alloy::{
    primitives::{Address, B256},
    signers::{local::PrivateKeySigner, Signer},
};
use bytes::Bytes;
use fynd_client::{
    AllowanceCheck, ApprovalParams, ClientFeeParams, EncodingOptions, ExecutionOptions,
    FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams, SignedApproval, SignedSwap,
    SigningHints,
};
use num_bigint::BigUint;

const DEFAULT_FYND_URL: &str = "http://localhost:3000";
const DEFAULT_RPC_URL: &str = "http://localhost:8545";
// Matches the key funded by scripts/dev-env.sh. Override with PRIVATE_KEY env var.
const DEV_PRIVATE_KEY: &str = "0x02d483ff876e4d1d55ddc829a22df2707bd2574ba18d0d870ef9c9edd3c0fe29";
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const SELL_AMOUNT: u128 = 1_000_000_000_000_000_000; // 1 WETH (18 decimals)
const SLIPPAGE: f64 = 0.01; // 1%
const FEE_BPS: u16 = 50; // 0.5% client fee

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fynd_url = std::env::var("FYND_URL").unwrap_or_else(|_| DEFAULT_FYND_URL.to_owned());
    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_owned());

    let private_key = std::env::var("PRIVATE_KEY").unwrap_or_else(|_| DEV_PRIVATE_KEY.to_owned());
    let sender_signer: PrivateKeySigner = private_key.parse()?;
    let sender = sender_signer.address();
    let sell_token: Address = WETH.parse()?;
    let buy_token: Address = USDC.parse()?;

    // Separate fee receiver key — in production this is the integrator's key.
    let fee_signer = PrivateKeySigner::random();
    let fee_receiver = fee_signer.address();

    let client = FyndClientBuilder::new(&fynd_url)
        .with_rpc_url(&rpc_url)
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

    let info = client.info().await?;
    let router_address = info
        .router_address()
        .expect("example requires a chain with encoding support")
        .clone();
    let chain_id = info.chain_id();

    // Approve the router to spend WETH if the current allowance is insufficient.
    if let Some(approval_payload) = client
        .approval(
            &ApprovalParams::new(
                Bytes::copy_from_slice(sell_token.as_slice()),
                BigUint::from(SELL_AMOUNT),
                AllowanceCheck::AtLeast(BigUint::from(SELL_AMOUNT)),
            ),
            &SigningHints::default(),
        )
        .await?
    {
        println!("Approving router to spend WETH...");
        let sig = sender_signer
            .sign_hash(&approval_payload.signing_hash())
            .await?;
        client
            .execute_approval(SignedApproval::assemble(approval_payload, sig))
            .await?
            .await?;
        println!("Approved.");
    }

    // [doc:start client-fee-rust]
    // Step 1: request a quote using unsigned client fee params.
    // The server encodes the full calldata and returns `swaps_hash`
    // in the fee breakdown and `signature_offset` in the transaction
    // so the client can patch the real signature in.
    let fee = ClientFeeParams::new(
        FEE_BPS,
        Bytes::copy_from_slice(fee_receiver.as_slice()),
        BigUint::ZERO,
        u64::MAX,
    );
    let order = Order::new(
        Bytes::copy_from_slice(sell_token.as_slice()),
        Bytes::copy_from_slice(buy_token.as_slice()),
        BigUint::from(SELL_AMOUNT),
        OrderSide::Sell,
        Bytes::copy_from_slice(sender.as_slice()),
        None,
    );
    let quote = client
        .quote(QuoteParams::new(
            order,
            QuoteOptions::default()
                .with_timeout_ms(5_000)
                .with_encoding_options(EncodingOptions::new(SLIPPAGE).with_client_fee(fee.clone())),
        ))
        .await?;

    let fee_breakdown = quote
        .fee_breakdown()
        .ok_or("no fee breakdown in quote")?;
    let swaps_hash = fee_breakdown
        .swaps_hash()
        .ok_or("no swaps_hash — server must support client fee signing")?;

    // Step 2: sign the full 10-field EIP-712 ClientFee hash.
    // receiver defaults to sender when the order has no explicit receiver.
    let hash = fee.eip712_signing_hash(
        chain_id,
        &router_address,
        quote.amount_in(),
        &Bytes::copy_from_slice(sell_token.as_slice()),
        &Bytes::copy_from_slice(buy_token.as_slice()),
        fee_breakdown.min_amount_received(),
        &Bytes::copy_from_slice(sender.as_slice()),
        swaps_hash,
    )?;
    let sig = fee_signer
        .sign_hash(&B256::from(hash))
        .await?;

    // Step 3: patch the real signature into the calldata.
    let quote = quote.with_client_fee_signature(&sig.as_bytes()[..])?;
    // [doc:end client-fee-rust]

    println!("amount_in:  {}", quote.amount_in());
    println!("amount_out: {}", quote.amount_out());

    // Sign and execute.
    let payload = client
        .swap_payload(quote, &SigningHints::default().with_simulate(true))
        .await?;
    let tx_sig = sender_signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let result = client
        .execute_swap(SignedSwap::assemble(payload, tx_sig), &ExecutionOptions::default())
        .await?
        .await?;

    println!("settled:    {:?} USDC", result.settled_amount());
    println!("gas cost:   {}", result.gas_cost());
    Ok(())
}
