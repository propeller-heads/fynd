//! Example: quote and dry-run a USDC → WETH swap using ERC-20 `transferFrom`.
//!
//! An ephemeral key is used and ERC-20 storage overrides inject a synthetic
//! balance so no real funds are required.
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
//! RPC_URL=http://localhost:8545 cargo run --example swap_erc20 -p fynd-client
//! ```

use alloy::{
    primitives::{keccak256, Address, B256, U256},
    signers::{local::PrivateKeySigner, Signer},
};
use bytes::Bytes;
use fynd_client::{
    ApprovalParams, EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide,
    QuoteOptions, QuoteParams, SignedApproval, SignedSwap, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;

const DEFAULT_FYND_URL: &str = "http://localhost:3000";
const DEFAULT_RPC_URL: &str = "http://localhost:8545";
// Matches the key funded by scripts/dev-env.sh. Override with PRIVATE_KEY env var.
const DEV_PRIVATE_KEY: &str = "0x912a64d0474cbddb4afd9b1aa2e800c433a3e975fa858395e6134220cf2b4cd5";
// 1000 USDC → WETH on Ethereum mainnet
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const SELL_AMOUNT: u128 = 1_000_000_000; // 1000 USDC (6 decimals)
const SLIPPAGE: f64 = 0.005; // 0.5%
                             // USDC storage layout (FiatTokenV2.1): balances at slot 9, allowances at slot 10.
const USDC_BALANCE_SLOT: u64 = 9;
const USDC_ALLOWANCE_SLOT: u64 = 10;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fynd_url = std::env::var("FYND_URL").unwrap_or_else(|_| DEFAULT_FYND_URL.to_owned());
    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_owned());

    let private_key = std::env::var("PRIVATE_KEY").unwrap_or_else(|_| DEV_PRIVATE_KEY.to_owned());
    let signer: PrivateKeySigner = private_key.parse()?;
    let sender = signer.address();
    let sell_token: Address = USDC.parse()?;
    let buy_token: Address = WETH.parse()?;

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

    // Check whether a router approval is needed and sign it if so.
    // In this dry-run example the broadcast is skipped — storage overrides inject the allowance
    // below. In production, remove the `let _ =` line and uncomment `execute_approval`.
    if let Some(approval_payload) = client
        .approval(
            &ApprovalParams::new(
                Bytes::copy_from_slice(sell_token.as_slice()),
                BigUint::from(SELL_AMOUNT),
                true, // check on-chain allowance first
            ),
            &SigningHints::default(),
        )
        .await?
    {
        println!("approval needed — signing");
        let approval_sig = signer
            .sign_hash(&approval_payload.signing_hash())
            .await?;
        let signed_approval = SignedApproval::assemble(approval_payload, approval_sig);
        // In production: client.execute_approval(signed_approval).await?.await?;
        let _ = signed_approval;
    } else {
        println!("allowance sufficient — skipping approval");
    }

    // Request a quote with ERC-20 encoding.
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

    let tx = quote
        .transaction()
        .ok_or("no calldata in quote")?;
    let router = Address::from_slice(tx.to().as_ref());

    // Sign the order.
    let payload = client
        .swap_payload(quote, &SigningHints::default())
        .await?;
    let sig = signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let signed = SignedSwap::assemble(payload, sig);

    // Dry-run: inject a synthetic ERC-20 balance and router allowance via state overrides.
    let max = Bytes::copy_from_slice(&B256::from(U256::MAX).0);
    let token_key = Bytes::copy_from_slice(sell_token.as_slice());
    let mut overrides = StorageOverrides::default();
    overrides.insert(
        token_key.clone(),
        Bytes::copy_from_slice(&mapping_slot(sender, USDC_BALANCE_SLOT).0),
        max.clone(),
    );
    overrides.insert(
        token_key,
        Bytes::copy_from_slice(&nested_mapping_slot(sender, router, USDC_ALLOWANCE_SLOT).0),
        max,
    );

    let result = client
        .execute_swap(
            signed,
            &ExecutionOptions { dry_run: true, storage_overrides: Some(overrides) },
        )
        .await?
        .await?;

    println!("gas: {}", result.gas_cost());
    Ok(())
}

/// Solidity mapping slot: `keccak256(abi.encode(key, base_slot))`.
fn mapping_slot(key: Address, base_slot: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_slice());
    buf[56..64].copy_from_slice(&base_slot.to_be_bytes());
    keccak256(buf)
}

/// Nested mapping slot: `keccak256(abi.encode(key2, mapping_slot(key1, base_slot)))`.
fn nested_mapping_slot(key1: Address, key2: Address, base_slot: u64) -> B256 {
    let inner = mapping_slot(key1, base_slot);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key2.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}
