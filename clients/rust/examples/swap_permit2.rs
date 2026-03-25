//! Example: sell 1 WETH for USDC using Permit2.
//!
//! Approves the Permit2 contract to spend WETH (if needed), signs the
//! Permit2 EIP-712 message, then executes a real swap via the local
//! Anvil fork started by the dev environment.
//!
//! Run with the local dev environment:
//!
//! ```sh
//! ./scripts/run-example.sh swap_permit2
//! ```
//!
//! Or manually after starting `./scripts/dev-env.sh`:
//!
//! ```sh
//! cargo run --example swap_permit2 -p fynd-client
//! ```

use alloy::{
    primitives::{Address, B256},
    signers::{local::PrivateKeySigner, Signer},
};
use bytes::Bytes;
use fynd_client::{
    ApprovalParams, EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide,
    PermitDetails as FyndPermitDetails, PermitSingle as FyndPermitSingle, QuoteOptions,
    QuoteParams, SignedApproval, SignedSwap, SigningHints, UserTransferType,
};
use num_bigint::BigUint;

const DEFAULT_FYND_URL: &str = "http://localhost:3000";
const DEFAULT_RPC_URL: &str = "http://localhost:8545";
// Matches the key funded by scripts/dev-env.sh. Override with PRIVATE_KEY env var.
const DEV_PRIVATE_KEY: &str = "0x02d483ff876e4d1d55ddc829a22df2707bd2574ba18d0d870ef9c9edd3c0fe29";
const PERMIT2: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";
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
    let permit2_addr: Address = PERMIT2.parse()?;

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

    let info = client.info().await?;
    let router = Address::from_slice(info.router_address().as_ref());
    let chain_id = info.chain_id();

    // Approve the Permit2 contract to spend WETH if the current allowance is insufficient.
    if let Some(approval_payload) = client
        .approval(
            &ApprovalParams::new(
                Bytes::copy_from_slice(sell_token.as_slice()),
                BigUint::from(SELL_AMOUNT),
                true,
            )
            .with_transfer_type(UserTransferType::TransferFromPermit2),
            &SigningHints::default(),
        )
        .await?
    {
        println!("Approving Permit2 to spend WETH...");
        let sig = signer
            .sign_hash(&approval_payload.signing_hash())
            .await?;
        client
            .execute_approval(SignedApproval::assemble(approval_payload, sig))
            .await?
            .await?;
        println!("Approved.");
    }

    // Sign the Permit2 EIP-712 message authorising the router to pull WETH.
    let permit = FyndPermitSingle::new(
        FyndPermitDetails::new(
            Bytes::copy_from_slice(sell_token.as_slice()),
            BigUint::from_bytes_be(&[0xFF; 20]), // uint160::MAX — unlimited allowance
            BigUint::from(281_474_976_710_655u64), // uint48::MAX — expiration
            BigUint::from(0u8),                  // nonce 0
        ),
        Bytes::copy_from_slice(router.as_slice()),
        BigUint::from(281_474_976_710_655u64), // sig_deadline
    );
    let permit_hash =
        permit.eip712_signing_hash(chain_id, &Bytes::copy_from_slice(permit2_addr.as_slice()))?;
    let permit_sig = Bytes::copy_from_slice(
        &signer
            .sign_hash(&B256::from(permit_hash))
            .await?
            .as_bytes(),
    );

    // Request a quote: sell 1 WETH for USDC with Permit2 encoding.
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
                .with_encoding_options(
                    EncodingOptions::new(SLIPPAGE).with_permit2(permit, permit_sig)?,
                ),
        ))
        .await?;

    println!("amount_in:  {}", quote.amount_in());
    println!("amount_out: {}", quote.amount_out());

    // Sign and execute.
    let payload = client
        .swap_payload(quote, &SigningHints::default())
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
