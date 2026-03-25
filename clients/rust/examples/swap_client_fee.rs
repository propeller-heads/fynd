//! Example: quote a USDC → WETH swap with a client fee.
//!
//! Demonstrates how to attach `ClientFeeParams` to a quote request so the
//! Tycho Router charges a client fee on the swap output.
//!
//! The fee receiver must sign an EIP-712 `ClientFee` message — this example
//! uses the same ephemeral key for both the sender and fee receiver.
//!
//! Run against a local Fynd instance:
//!
//! ```sh
//! cargo run --example swap_client_fee -p fynd-client
//! ```

use alloy::signers::{local::PrivateKeySigner, Signer};
use bytes::Bytes;
use fynd_client::{
    ClientFeeParams, EncodingOptions, FyndClientBuilder, Order, OrderSide, QuoteOptions,
    QuoteParams,
};
use num_bigint::BigUint;

const FYND_URL: &str = "http://localhost:3000";
const RPC_URL: &str = "https://eth.llamarpc.com";
const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const SELL_AMOUNT: u128 = 1_000_000_000; // 1000 USDC (6 decimals)
const SLIPPAGE: f64 = 0.005; // 0.5%
const FEE_BPS: u16 = 50; // 0.5% client fee

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();

    let client = FyndClientBuilder::new(FYND_URL, RPC_URL)
        .with_sender(sender)
        .build()
        .await?;

    // The fee receiver signs the fee params. In production this would be a
    // separate key controlled by the integrator; here we reuse the sender key.
    let fee_receiver = sender;
    let fee_receiver_bytes = Bytes::copy_from_slice(fee_receiver.as_slice());
    let max_contribution = BigUint::ZERO;
    // In production the deadline would be a short period of time.
    let deadline = BigUint::from(u64::MAX);

    // First, get a quote to discover the router address.
    // TODO: fix this after the router address is exposed
    let quote = client
        .quote(QuoteParams::new(
            Order::new(
                Bytes::copy_from_slice(
                    USDC.parse::<alloy::primitives::Address>()?
                        .as_slice(),
                ),
                Bytes::copy_from_slice(
                    WETH.parse::<alloy::primitives::Address>()?
                        .as_slice(),
                ),
                BigUint::from(SELL_AMOUNT),
                OrderSide::Sell,
                Bytes::copy_from_slice(sender.as_slice()),
                None,
            ),
            QuoteOptions::default().with_timeout_ms(5_000),
        ))
        .await?;

    let router_address = quote
        .transaction()
        .map(|tx| Bytes::copy_from_slice(tx.to().as_ref()))
        .unwrap_or_else(|| {
            // Fallback: use a placeholder if no tx was returned (quote-only mode).
            Bytes::copy_from_slice(&[0u8; 20])
        });

    // [doc:start client-fee-rust]
    // Compute the EIP-712 signing hash for the client fee.
    let hash = ClientFeeParams::eip712_signing_hash(
        FEE_BPS,
        &fee_receiver_bytes,
        &max_contribution,
        &deadline,
        1, // chainId = Ethereum mainnet
        &router_address,
    )?;

    // Sign the hash with the fee receiver's key.
    let sig = signer
        .sign_hash(&alloy::primitives::B256::from(hash))
        .await?;
    let signature = Bytes::copy_from_slice(&sig.as_bytes()[..]);

    // Build encoding options with the client fee attached.
    let fee =
        ClientFeeParams::new(FEE_BPS, fee_receiver_bytes, max_contribution, deadline, signature);
    let encoding_options = EncodingOptions::new(SLIPPAGE).with_client_fee(fee);
    // [doc:end client-fee-rust]

    // Request a quote with the client fee.
    let quote = client
        .quote(QuoteParams::new(
            Order::new(
                Bytes::copy_from_slice(
                    USDC.parse::<alloy::primitives::Address>()?
                        .as_slice(),
                ),
                Bytes::copy_from_slice(
                    WETH.parse::<alloy::primitives::Address>()?
                        .as_slice(),
                ),
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

    println!("Status:     {:?}", quote.status());
    println!("Amount in:  {}", quote.amount_in());
    println!("Amount out: {}", quote.amount_out());
    if quote.transaction().is_some() {
        println!(
            "Calldata:   present ({} bytes)",
            quote
                .transaction()
                .unwrap()
                .data()
                .len()
        );
    }

    Ok(())
}
