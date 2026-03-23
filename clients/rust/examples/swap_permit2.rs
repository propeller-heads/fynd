//! Example: quote and dry-run a USDC → WETH swap using Permit2.
//!
//! An ephemeral key is used (nonce 0 requires no Permit2 chain state) and
//! ERC-20 storage overrides inject a synthetic balance and allowance to the
//! Permit2 contract. No real funds or on-chain approvals are required.
//!
//! Run against a local Fynd instance:
//!
//! ```sh
//! cargo run --example swap_permit2 -p fynd-client
//! ```

use alloy::{
    network::Ethereum,
    primitives::{keccak256, Address, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    signers::{local::PrivateKeySigner, Signer},
};
use bytes::Bytes;
use fynd_client::{
    EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide,
    PermitDetails as FyndPermitDetails, PermitSingle as FyndPermitSingle, QuoteOptions,
    QuoteParams, SignedSwap, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;

const FYND_URL: &str = "http://localhost:3000";
const RPC_URL: &str = "https://eth.llamarpc.com";
const PERMIT2: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";
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
    // Ephemeral key — nonce 0 requires no Permit2 chain state for the dry-run.
    let signer = PrivateKeySigner::random();
    let sender = signer.address();

    let provider: RootProvider<Ethereum> =
        ProviderBuilder::default().connect_http(RPC_URL.parse::<reqwest::Url>()?);
    let sell_token: Address = USDC.parse()?;
    let buy_token: Address = WETH.parse()?;
    let permit2_addr: Address = PERMIT2.parse()?;

    let client = FyndClientBuilder::new(FYND_URL, RPC_URL)
        .with_sender(sender)
        .build()
        .await?;

    // Discover the router address from a plain ERC-20 quote — it becomes the Permit2 spender.
    let router = {
        let q = client
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
        Address::from_slice(
            q.transaction()
                .ok_or("no calldata in quote")?
                .to()
                .as_ref(),
        )
    };

    // Build and sign the Permit2 EIP-712 message off-chain.
    // Dry-run: nonce 0, uint48::MAX deadlines — no chain reads needed.
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
    let chain_id = provider.get_chain_id().await?;
    let permit_hash =
        permit.eip712_signing_hash(chain_id, &Bytes::copy_from_slice(permit2_addr.as_slice()))?;
    let permit_sig = Bytes::copy_from_slice(
        &signer
            .sign_hash(&B256::from(permit_hash))
            .await?
            .as_bytes(),
    );

    // Request a quote with Permit2 encoding.
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

    // Sign the order.
    let payload = client
        .swap_payload(quote, &SigningHints::default())
        .await?;
    let sig = signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let signed = SignedSwap::assemble(payload, sig);

    // Dry-run: inject synthetic ERC-20 balance and allowance to the Permit2 contract.
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
        Bytes::copy_from_slice(&nested_mapping_slot(sender, permit2_addr, USDC_ALLOWANCE_SLOT).0),
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
