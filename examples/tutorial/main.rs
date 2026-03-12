//! Tutorial Example: Quote and Execute a Swap via FyndClient
//!
//! This example demonstrates how to:
//! 1. Build a FyndClient and check solver health
//! 2. Request a swap quote with server-side calldata encoding
//! 3. Display the route and pricing
//! 4. Execute the swap — dry-run (default) or on-chain (--execute)
//!
//! Dry-run uses an ephemeral key and ERC-20 storage overrides; no funds required.
//! On-chain execution requires `PRIVATE_KEY` env var and a funded wallet.

use std::{env, str::FromStr, time::Duration};

use alloy::hex;
use alloy::primitives::{Address, Bytes as AlloyBytes, Keccak256, TxKind, B256, U256};
use alloy::network::Ethereum;
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::{local::PrivateKeySigner, Signer};
use bytes::Bytes;
use clap::Parser;
use fynd_client::{
    EncodingOptions, ExecutionOptions, FyndClientBuilder, Order, OrderSide, QuoteOptions,
    QuoteParams, SignedOrder, SigningHints, StorageOverrides,
};
use num_bigint::BigUint;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Tutorial CLI: Quote and execute a swap via the Fynd solver
#[derive(Parser)]
#[command(name = "tutorial")]
#[command(about = "Get a quote from the Fynd solver and execute the swap (dry-run by default)")]
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
    let rpc_url = env::var("RPC_URL")
        .map_err(|_| "RPC_URL environment variable is required")?;

    // Load or generate signer. Real execution requires PRIVATE_KEY; dry-run uses an
    // ephemeral key because storage overrides inject the balance on-the-fly.
    let signer = if cli.execute {
        let pk_hex = env::var("PRIVATE_KEY")
            .map_err(|_| "--execute requires PRIVATE_KEY environment variable")?;
        let pk_bytes = B256::from_str(&pk_hex)
            .map_err(|e| format!("invalid PRIVATE_KEY: {e}"))?;
        PrivateKeySigner::from_bytes(&pk_bytes)
            .map_err(|e| format!("invalid PRIVATE_KEY: {e}"))?
    } else {
        PrivateKeySigner::random()
    };
    let sender = signer.address();
    info!("Sender: {:?}", sender);

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

    // Parse token addresses
    let sell_token_addr = Address::from_str(&cli.sell_token)
        .map_err(|e| format!("invalid sell token address: {e}"))?;
    let buy_token_addr = Address::from_str(&cli.buy_token)
        .map_err(|e| format!("invalid buy token address: {e}"))?;

    let sell_token_bytes = Bytes::copy_from_slice(sell_token_addr.as_slice());
    let buy_token_bytes = Bytes::copy_from_slice(buy_token_addr.as_slice());
    let sender_bytes = Bytes::copy_from_slice(sender.as_slice());

    let amount = BigUint::from(cli.sell_amount);
    let slippage = cli.slippage_bps as f64 / 10_000.0;

    info!(
        "Requesting quote: {} atomic units of {} -> {}",
        cli.sell_amount, cli.sell_token, cli.buy_token
    );

    let order = Order::new(
        sell_token_bytes,
        buy_token_bytes,
        amount,
        OrderSide::Sell,
        sender_bytes,
        None,
    );
    let options = QuoteOptions::default()
        .with_timeout_ms(5_000)
        .with_encoding_options(EncodingOptions::new(slippage));
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

    // Extract router address before signable_payload consumes the quote
    let tx = quote
        .transaction()
        .ok_or("Quote has no calldata. Ensure encoding_options were set in the request.")?;
    let router = Address::from_slice(tx.to().as_ref());

    // On-chain execution only: verify the router is approved before signing, so
    // the user gets a clear error and a fix command rather than a cryptic revert.
    if cli.execute {
        let allowance = read_erc20_allowance(&rpc_url, sell_token_addr, sender, router).await?;
        let required = BigUint::from(cli.sell_amount);
        if allowance < required {
            eprintln!("\nError: insufficient sell-token allowance for the Fynd router.");
            eprintln!("  Token:     {:#x}", sell_token_addr);
            eprintln!("  Router:    {:#x}", router);
            eprintln!("  Allowance: {}", allowance);
            eprintln!("  Required:  {}", required);
            eprintln!("\nApprove the router with:");
            eprintln!(
                "  cast send {:#x} \"approve(address,uint256)\" {:#x} \\\n    \
                 $(cast max-uint256) --rpc-url $RPC_URL --private-key $PRIVATE_KEY",
                sell_token_addr, router,
            );
            return Err("insufficient sell-token allowance".into());
        }
    }

    // Build and sign the payload
    let payload = client.signable_payload(quote, &SigningHints::default()).await?;
    let signature = signer.sign_hash(&payload.signing_hash()).await?;
    let signed = SignedOrder::assemble(payload, signature);

    let exec_options = if cli.execute {
        info!("Submitting on-chain transaction...");
        ExecutionOptions::default()
    } else {
        // Dry-run: inject unlimited sell-token balance + allowance via storage overrides
        // so the simulation succeeds without real funds.
        let mut overrides = StorageOverrides::default();
        let max_val = Bytes::copy_from_slice(&B256::from(U256::MAX).0);
        let token_key = Bytes::copy_from_slice(sell_token_addr.as_slice());
        overrides.insert(
            token_key.clone(),
            Bytes::copy_from_slice(&erc20_balance_slot(sender).0),
            max_val.clone(),
        );
        overrides.insert(
            token_key,
            Bytes::copy_from_slice(&erc20_allowance_slot(sender, router).0),
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
    rpc_url: &str,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<BigUint, Box<dyn std::error::Error>> {
    // Encode allowance(address,address) calldata manually — 4-byte selector + two padded addresses
    let mut hasher = Keccak256::new();
    hasher.update(b"allowance(address,address)");
    let full_hash = hasher.finalize();
    let mut calldata = full_hash[..4].to_vec();
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(owner.as_slice());
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(spender.as_slice());

    let provider: RootProvider<Ethereum> = ProviderBuilder::default()
        .connect_http(rpc_url.parse::<reqwest::Url>()?);
    let req = TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: AlloyBytes::from(calldata).into(),
        ..Default::default()
    };
    let result = provider.call(req).await?;
    if result.len() < 32 {
        return Err(format!("allowance() returned {} bytes, expected 32", result.len()).into());
    }
    Ok(BigUint::from_bytes_be(&result[..32]))
}

/// Compute the ERC-20 `balances` mapping slot for `holder` (mapping at storage position 1).
///
/// Slot = `keccak256(holder_padded_to_32_bytes || uint256(1))`
fn erc20_balance_slot(holder: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[63] = 1;
    let mut hasher = Keccak256::new();
    hasher.update(buf);
    hasher.finalize()
}

/// Compute the ERC-20 `allowances` mapping slot for `allowances[from][to]` (mapping at position 2).
///
/// Inner slot = `keccak256(from_padded || uint256(2))`
/// Outer slot = `keccak256(to_padded   || inner_slot)`
fn erc20_allowance_slot(from: Address, to: Address) -> B256 {
    let mut inner_buf = [0u8; 64];
    inner_buf[12..32].copy_from_slice(from.as_slice());
    inner_buf[63] = 2;
    let mut inner_hasher = Keccak256::new();
    inner_hasher.update(inner_buf);
    let inner: B256 = inner_hasher.finalize();

    let mut outer_buf = [0u8; 64];
    outer_buf[12..32].copy_from_slice(to.as_slice());
    outer_buf[32..64].copy_from_slice(inner.as_slice());
    let mut outer_hasher = Keccak256::new();
    outer_hasher.update(outer_buf);
    outer_hasher.finalize()
}
