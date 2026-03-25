//! Example: quote and dry-run a USDC → WETH swap with a client fee.
//!
//! Demonstrates how to attach `ClientFeeParams` to a quote request so the
//! Tycho Router charges a client fee on the swap output.
//!
//! Two ephemeral keys are used: one for the sender and one for the fee receiver.
//! ERC-20 storage overrides inject a synthetic balance so no real funds are needed.
//!
//! Run with the local dev environment:
//!
//! ```sh
//! ./scripts/run-example.sh swap_client_fee
//! ```
//!
//! Or manually after starting `./scripts/dev-env.sh`:
//!
//! ```sh
//! RPC_URL=http://localhost:8545 cargo run --example swap_client_fee -p fynd-client
//! ```

use alloy::{
    primitives::{keccak256, Address, B256, U256},
    signers::{local::PrivateKeySigner, Signer},
};
use bytes::Bytes;
use fynd_client::{
    ClientFeeParams, EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide,
    QuoteOptions, QuoteParams, SignedSwap, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;

const DEFAULT_FYND_URL: &str = "http://localhost:3000";
const DEFAULT_RPC_URL: &str = "http://localhost:8545";
// Matches the key funded by scripts/dev-env.sh. Override with PRIVATE_KEY env var.
const DEV_PRIVATE_KEY: &str = "0x02d483ff876e4d1d55ddc829a22df2707bd2574ba18d0d870ef9c9edd3c0fe29";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const SELL_AMOUNT: u128 = 1_000_000_000; // 1000 USDC (6 decimals)
const SLIPPAGE: f64 = 0.005; // 0.5%
const FEE_BPS: u16 = 50; // 0.5% client fee

// USDC storage layout (FiatTokenV2.1): balances at slot 9, allowances at slot 10.
const USDC_BALANCE_SLOT: u64 = 9;
const USDC_ALLOWANCE_SLOT: u64 = 10;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fynd_url = std::env::var("FYND_URL").unwrap_or_else(|_| DEFAULT_FYND_URL.to_owned());
    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_owned());

    let private_key = std::env::var("PRIVATE_KEY").unwrap_or_else(|_| DEV_PRIVATE_KEY.to_owned());
    let sender_signer: PrivateKeySigner = private_key.parse()?;
    let sender = sender_signer.address();
    let sell_token: Address = USDC.parse()?;
    let buy_token: Address = WETH.parse()?;

    // Separate fee receiver key — in production this is the integrator's key.
    let fee_signer = PrivateKeySigner::random();
    let fee_receiver = fee_signer.address();

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
    let router_address = info.router_address().clone();
    let chain_id = info.chain_id();

    // [doc:start client-fee-rust]
    // Build the fee params (without signature).
    let fee = ClientFeeParams::new(
        FEE_BPS,
        Bytes::copy_from_slice(fee_receiver.as_slice()),
        BigUint::ZERO,
        u64::MAX,
    );

    // Compute the EIP-712 signing hash and sign it with the fee receiver's key.
    let hash = fee.eip712_signing_hash(chain_id, &router_address)?;
    let sig = fee_signer
        .sign_hash(&B256::from(hash))
        .await?;

    // Attach the signature and wire it into encoding options.
    let fee = fee.with_signature(Bytes::copy_from_slice(&sig.as_bytes()[..]));
    let encoding_options = EncodingOptions::new(SLIPPAGE).with_client_fee(fee);
    // [doc:end client-fee-rust]

    // Request a quote with the client fee.
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
                .with_encoding_options(encoding_options),
        ))
        .await?;

    println!("amount_in:  {}", quote.amount_in());
    println!("amount_out: {}", quote.amount_out());

    // Sign the order.
    let payload = client
        .swap_payload(quote, &SigningHints::default())
        .await?;
    let tx_sig = sender_signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let signed = SignedSwap::assemble(payload, tx_sig);

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
        Bytes::copy_from_slice(
            &nested_mapping_slot(sender, Address::from_slice(&router_address), USDC_ALLOWANCE_SLOT)
                .0,
        ),
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
