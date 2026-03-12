//! Permit2 Example: Quote and Execute a Swap using Permit2 Token Authorization
//!
//! This example mirrors the tutorial but uses Permit2 (AllowanceTransfer) instead
//! of a standard ERC-20 `approve` + `transferFrom`.
//!
//! With Permit2, you sign an off-chain EIP-712 message granting the Tycho Router a
//! temporary, nonce-protected allowance — no approve transaction is required if the
//! Permit2 contract already has an unlimited ERC-20 approval (the common wallet default).
//!
//! # Dry-run (default)
//!
//! Uses an ephemeral key and ERC-20 storage overrides so no funds are required.
//! Because the ephemeral address has never interacted with Permit2, its nonce is 0
//! and the generated signature is valid without any chain-state setup.
//!
//! # On-chain execution (`--execute`)
//!
//! Requires `PRIVATE_KEY`, a funded wallet, and an existing ERC-20 approval from
//! your address to the Permit2 contract (not the router):
//!
//! ```sh
//! cast send <TOKEN> "approve(address,uint256)" \
//!   0x000000000022D473030F116dDEE9F6B43aC78BA3 \
//!   115792089237316195423570985008687907853269984665640564039457584007913129639935 \
//!   --rpc-url $RPC_URL --private-key $PRIVATE_KEY
//! ```

use std::{env, str::FromStr, time::Duration};

use alloy::{
    hex,
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::TransactionRequest,
    signers::{local::PrivateKeySigner, Signer},
    sol_types::SolCall,
};
use bytes::Bytes;
use clap::Parser;
use fynd_client::{
    EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide,
    PermitDetails as FyndPermitDetails, PermitSingle as FyndPermitSingle, QuoteOptions,
    QuoteParams, SignedOrder, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod erc20;
mod permit2;

use erc20::IERC20;
use permit2::PERMIT2_ADDRESS;

/// Max uint160 — used as the Permit2 approved amount (unlimited).
fn max_uint160() -> BigUint {
    // 20 bytes of 0xFF = 2^160 - 1
    BigUint::from_bytes_be(&[0xFF; 20])
}

/// Permit2 tutorial: quote and execute a swap using Permit2 token authorization.
#[derive(Parser)]
#[command(name = "permit2")]
#[command(about = "Get a Fynd quote and execute the swap via Permit2 (no approve tx needed)")]
struct Cli {
    /// Sell token address (defaults to USDC on mainnet)
    #[arg(long, default_value = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    sell_token: String,

    /// Buy token address (defaults to WETH on mainnet)
    #[arg(long, default_value = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    buy_token: String,

    /// Amount to sell in raw atomic units (e.g. 1000000000 for 1000 USDC at 6 decimals)
    #[arg(long, default_value_t = 1_000_000_000u128)]
    sell_amount: u128,

    /// Solver API URL
    #[arg(long, default_value = "http://localhost:3000")]
    fynd_url: String,

    /// Slippage tolerance in basis points (default: 50 = 0.5%)
    #[arg(long, default_value_t = 50u32)]
    slippage_bps: u32,

    /// Tycho Router contract address — this is the Permit2 spender.
    /// Consult Fynd documentation for the correct address on your network.
    #[arg(long)]
    router: String,

    /// Permit2 contract address (defaults to the canonical cross-chain deployment)
    #[arg(long, default_value = "0x000000000022D473030F116dDEE9F6B43aC78BA3")]
    permit2: String,

    /// Submit the swap on-chain instead of dry-running it.
    /// Requires the PRIVATE_KEY environment variable to be set.
    #[arg(long)]
    execute: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let rpc_url = env::var("RPC_URL").map_err(|_| "RPC_URL environment variable is required")?;

    // Load or generate signer. Real execution requires PRIVATE_KEY; dry-run uses an
    // ephemeral key — the ephemeral address has nonce 0 in Permit2 so no chain setup
    // is needed, and ERC-20 balance/allowance are injected via storage overrides.
    let signer = if cli.execute {
        let pk_hex = env::var("PRIVATE_KEY")
            .map_err(|_| "--execute requires PRIVATE_KEY environment variable")?;
        let pk_bytes = B256::from_str(&pk_hex).map_err(|e| format!("invalid PRIVATE_KEY: {e}"))?;
        PrivateKeySigner::from_bytes(&pk_bytes).map_err(|e| format!("invalid PRIVATE_KEY: {e}"))?
    } else {
        PrivateKeySigner::random()
    };
    let sender = signer.address();
    info!("Sender: {:?}", sender);

    let provider: RootProvider<Ethereum> =
        ProviderBuilder::default().connect_http(rpc_url.parse::<reqwest::Url>()?);

    let client = FyndClientBuilder::new(&cli.fynd_url, &rpc_url)
        .with_sender(sender)
        .build()
        .await?;

    // Health check
    info!("Checking solver health at {}...", cli.fynd_url);
    let health = client.health().await?;
    info!(
        "Solver healthy: {}, last update: {}ms ago, {} solver pools",
        health.healthy(),
        health.last_update_ms(),
        health.num_solver_pools(),
    );
    if !health.healthy() {
        return Err("Solver is not healthy. Please wait for market data to load.".into());
    }

    // Parse addresses
    let sell_token_addr = Address::from_str(&cli.sell_token)
        .map_err(|e| format!("invalid sell token address: {e}"))?;
    let buy_token_addr = Address::from_str(&cli.buy_token)
        .map_err(|e| format!("invalid buy token address: {e}"))?;
    let router_addr =
        Address::from_str(&cli.router).map_err(|e| format!("invalid router address: {e}"))?;
    let permit2_addr =
        Address::from_str(&cli.permit2).map_err(|e| format!("invalid permit2 address: {e}"))?;

    let sell_token_bytes = Bytes::copy_from_slice(sell_token_addr.as_slice());
    let buy_token_bytes = Bytes::copy_from_slice(buy_token_addr.as_slice());
    let sender_bytes = Bytes::copy_from_slice(sender.as_slice());

    let amount = BigUint::from(cli.sell_amount);
    let slippage = cli.slippage_bps as f64 / 10_000.0;

    // On-chain execution: verify the Permit2 contract is approved by the user, then
    // read the current nonce. Dry-run: use nonce 0 and a far-future deadline since
    // the ephemeral key has never interacted with Permit2.
    let (nonce, expiration, sig_deadline) = if cli.execute {
        let allowance =
            read_erc20_allowance(&provider, sell_token_addr, sender, permit2_addr).await?;
        if allowance < amount {
            eprintln!("\nError: insufficient ERC-20 allowance to the Permit2 contract.");
            eprintln!("  Token:     {:#x}", sell_token_addr);
            eprintln!("  Permit2:   {:#x}", permit2_addr);
            eprintln!("  Allowance: {}", allowance);
            eprintln!("  Required:  {}", amount);
            eprintln!("\nApprove Permit2 with:");
            eprintln!(
                "  cast send {:#x} \"approve(address,uint256)\" {:#x} {} \\\n    \
                 --rpc-url $RPC_URL --private-key $PRIVATE_KEY",
                sell_token_addr,
                permit2_addr,
                u128::MAX,
            );
            return Err("insufficient allowance to Permit2".into());
        }

        let nonce =
            permit2::read_nonce(&provider, permit2_addr, sender, sell_token_addr, router_addr)
                .await?;
        info!("Permit2 nonce for sender: {}", nonce);

        let block = provider
            .get_block_by_number(Default::default())
            .await?
            .ok_or("could not fetch latest block")?;
        let now = block.header.timestamp;
        (nonce, now + 3_600, now + 1_800) // expiration: +1h, sig_deadline: +30m
    } else {
        // Dry-run: ephemeral key, nonce is naturally 0, use max uint48 as deadline.
        // 2^48 - 1 = max uint48, well past any realistic expiry
        (0u64, 281_474_976_710_655u64, 281_474_976_710_655u64)
    };

    // Build the fynd-client permit struct — this is also the value that gets signed.
    let fynd_permit = FyndPermitSingle::new(
        FyndPermitDetails::new(
            Bytes::copy_from_slice(sell_token_addr.as_slice()),
            max_uint160(),
            BigUint::from(expiration),
            BigUint::from(nonce),
        ),
        Bytes::copy_from_slice(router_addr.as_slice()),
        BigUint::from(sig_deadline),
    );

    // Compute the Permit2 EIP-712 hash and sign it off-chain.
    let chain_id = provider.get_chain_id().await?;
    info!("Signing Permit2 EIP-712 hash (chain_id={}, nonce={})...", chain_id, nonce);
    let permit2_addr_bytes = Bytes::copy_from_slice(permit2_addr.as_slice());
    let signing_hash = fynd_permit.eip712_signing_hash(chain_id, &permit2_addr_bytes)?;
    let sig = signer.sign_hash(&alloy::primitives::B256::from(signing_hash)).await?;
    let signature = Bytes::copy_from_slice(&sig.as_bytes());

    info!(
        "Requesting quote: {} atomic units of {} -> {}",
        cli.sell_amount, cli.sell_token, cli.buy_token
    );

    let order =
        Order::new(sell_token_bytes, buy_token_bytes, amount, OrderSide::Sell, sender_bytes, None);
    let options = QuoteOptions::default()
        .with_timeout_ms(5_000)
        .with_encoding_options(EncodingOptions::new(slippage).with_permit2(fynd_permit, signature)?);
    let quote = client.quote(QuoteParams::new(order, options)).await?;

    // Display quote
    println!("\n========== Quote ==========");
    println!("Status:       {:?}", quote.status());
    println!("Amount in:    {}", quote.amount_in());
    println!("Amount out:   {}", quote.amount_out());
    println!("Gas estimate: {}", quote.gas_estimate());
    if let Some(impact) = quote.price_impact_bps() {
        println!("Price impact: {:.2}%", impact as f64 / 100.0);
    }
    println!("Solve time:   {}ms", quote.solve_time_ms());
    if let Some(route) = quote.route() {
        println!("Route ({} hops):", route.swaps().len());
        for (i, swap) in route.swaps().iter().enumerate() {
            println!(
                "  {}. {} -> {} via {} (pool: {})",
                i + 1,
                hex::encode(swap.token_in()),
                hex::encode(swap.token_out()),
                swap.protocol(),
                swap.component_id(),
            );
        }
    }
    println!("============================\n");

    // Build and sign the Fynd order payload
    let payload = client.signable_payload(quote, &SigningHints::default()).await?;
    let order_sig = signer.sign_hash(&payload.signing_hash()).await?;
    let signed = SignedOrder::assemble(payload, order_sig);

    let exec_options = if cli.execute {
        info!("Submitting on-chain transaction...");
        ExecutionOptions::default()
    } else {
        // Dry-run: inject ERC-20 balance and allowance-to-Permit2 via storage overrides.
        // No Permit2 state override is needed because the ephemeral key's nonce is 0
        // and the EIP-712 signature is valid.
        info!("Detecting storage slots for {}...", cli.sell_token);
        let (balance_slot_result, allowance_slot_result) = tokio::join!(
            erc20::find_balance_slot(&provider, sell_token_addr, sender),
            erc20::find_allowance_slot(&provider, sell_token_addr, sender, PERMIT2_ADDRESS),
        );
        let balance_pos = balance_slot_result?;
        let allowance_pos = allowance_slot_result?;
        info!(
            "Found balance slot {} and allowance-to-Permit2 slot {}",
            balance_pos, allowance_pos
        );

        let max_val = Bytes::copy_from_slice(&B256::from(U256::MAX).0);
        let token_key = Bytes::copy_from_slice(sell_token_addr.as_slice());
        let mut overrides = StorageOverrides::default();
        overrides.insert(
            token_key.clone(),
            Bytes::copy_from_slice(&erc20::balance_slot_at(sender, balance_pos).0),
            max_val.clone(),
        );
        overrides.insert(
            token_key,
            Bytes::copy_from_slice(
                &erc20::allowance_slot_at(sender, PERMIT2_ADDRESS, allowance_pos).0,
            ),
            max_val,
        );
        info!("Submitting dry-run execution...");
        ExecutionOptions { dry_run: true, storage_overrides: Some(overrides) }
    };

    let receipt = client.execute(signed, &exec_options).await?;

    let settled = if cli.execute {
        tokio::time::timeout(Duration::from_secs(120), receipt)
            .await
            .map_err(|_| "timed out waiting for transaction to be mined")??
    } else {
        receipt.await?
    };

    if cli.execute {
        println!("\nSwap executed on-chain!");
    } else {
        println!("\nSimulation successful!");
    }
    match settled.settled_amount() {
        Some(amount) => println!("Settled amount: {}", amount),
        None => println!("Settled amount: (no matching Transfer log)"),
    }
    println!("Gas cost (wei): {}", settled.gas_cost());

    Ok(())
}

/// Read the current `allowance(owner, spender)` from an ERC-20 token via `eth_call`.
async fn read_erc20_allowance(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<BigUint, Box<dyn std::error::Error>> {
    let calldata = IERC20::allowanceCall { owner, spender }.abi_encode();
    let result = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(token)),
            input: AlloyBytes::from(calldata).into(),
            ..Default::default()
        })
        .await?;
    if result.len() < 32 {
        return Err(format!("allowance() returned {} bytes, expected 32", result.len()).into());
    }
    Ok(BigUint::from_bytes_be(&result[..32]))
}
