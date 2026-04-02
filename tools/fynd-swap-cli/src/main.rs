//! fynd-swap-cli — quote and execute token swaps via the Fynd solver.
//!
//! Supports both ERC-20 approve and Permit2 transfer flows.
//!
//! # Dry-run (default)
//!
//! Uses a well-funded sender address and ERC-20 storage overrides so no funds are required.
//!
//! # On-chain execution (`--execute`)
//!
//! Requires `PRIVATE_KEY` env var and a funded wallet. Any missing approvals are submitted
//! automatically before the swap.

use std::{env, str::FromStr, time::Duration};

use alloy::{
    hex,
    network::Ethereum,
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    signers::{local::PrivateKeySigner, Signer},
};
use anyhow::{bail, Context};
use bytes::Bytes;
use clap::Parser;
use fynd_client::{
    AllowanceCheck, ApprovalParams, EncodingOptions, ExecutionOptions, FyndClient,
    FyndClientBuilder, HealthStatus, Order, OrderSide, PermitDetails as FyndPermitDetails,
    PermitSingle as FyndPermitSingle, QuoteOptions, QuoteParams, SignedApproval, SignedSwap,
    SigningHints, StorageOverrides, UserTransferType,
};
use num_bigint::BigUint;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod erc20;
mod permit2;

// ─── CLI ─────────────────────────────────────────────────────────────────────

/// Token transfer flow — mirrors `UserTransferType` from `fynd-rpc-types`.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum TransferType {
    /// Standard ERC-20 approval + transferFrom.
    TransferFrom,
    /// Permit2 off-chain signature flow.
    TransferFromPermit2,
    /// Use funds already deposited in the Tycho Router vault.
    UseVaultsFunds,
}

/// fynd-swap-cli — quote and execute token swaps via the Fynd solver.
#[derive(Parser)]
#[command(name = "fynd-swap-cli")]
#[command(about = "Quote and execute token swaps via Fynd (ERC-20 or Permit2)")]
struct Cli {
    /// Sell token address (defaults to WETH on mainnet)
    #[arg(long, default_value = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    sell_token: String,

    /// Buy token address (defaults to USDC on mainnet)
    #[arg(long, default_value = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    buy_token: String,

    /// Amount to sell in raw atomic units (e.g. 1000000000 for 1000 USDC at 6 decimals)
    #[arg(long, default_value_t = 1000000000000000000u128)]
    sell_amount: u128,

    /// Slippage tolerance in basis points (e.g. 50 = 0.5%)
    #[arg(long, default_value_t = 50u32)]
    slippage_bps: u32,

    /// Fynd solver URL
    #[arg(long, env = "FYND_URL", default_value = "http://localhost:3000")]
    fynd_url: String,

    /// Token transfer flow
    #[arg(long, default_value = "transfer-from")]
    transfer_type: TransferType,

    /// Submit the swap on-chain instead of dry-running it.
    /// Requires the PRIVATE_KEY environment variable.
    #[arg(long)]
    execute: bool,

    /// Permit2 contract address (defaults to the canonical cross-chain deployment)
    #[arg(long, default_value = "0x000000000022D473030F116dDEE9F6B43aC78BA3")]
    permit2: String,

    /// Node RPC URL for the target chain
    #[arg(long, env = "RPC_URL", default_value = "https://reth-ethereum.ithaca.xyz/rpc")]
    rpc_url: String,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Max uint160 — used as the Permit2 approved amount (unlimited).
fn max_uint160() -> BigUint {
    BigUint::from_bytes_be(&[0xFF; 20])
}

/// Detect ERC-20 storage slots and build `StorageOverrides` for a dry-run.
///
/// Injects a large sentinel into the holder's balance slot and the `holder → spender`
/// allowance slot so the simulation succeeds without real funds. Uses `U256::MAX >> 1`
/// rather than `U256::MAX` to avoid triggering tokens that pack metadata into bit 255
/// (e.g. USDC uses that bit as a blacklist flag).
async fn build_dry_run_overrides(
    provider: &RootProvider<Ethereum>,
    sell_token: Address,
    sender: Address,
    spender: Address,
) -> anyhow::Result<StorageOverrides> {
    info!("Detecting storage slots for {sell_token:#x}...");
    let (balance_res, allowance_res) = tokio::join!(
        erc20::find_balance_slot(provider, sell_token, sender),
        erc20::find_allowance_slot(provider, sell_token, sender, spender),
    );
    let balance_pos = balance_res?;
    let allowance_pos = allowance_res?;
    info!("Found balance slot {balance_pos} and allowance slot {allowance_pos}");

    // Use MAX >> 1 (clear the top bit) to avoid triggering tokens that pack metadata into
    // bit 255 of the storage slot — e.g. USDC uses the top bit as a blacklist flag.
    // 2^255 - 1 is still large enough to cover any realistic balance or allowance.
    let max_val = Bytes::copy_from_slice(&B256::from(U256::MAX >> 1).0);
    let token_key = Bytes::copy_from_slice(sell_token.as_slice());
    let mut overrides = StorageOverrides::default();
    overrides.insert(
        token_key.clone(),
        Bytes::copy_from_slice(&erc20::balance_slot_at(sender, balance_pos).0),
        max_val.clone(),
    );
    overrides.insert(
        token_key,
        Bytes::copy_from_slice(&erc20::allowance_slot_at(sender, spender, allowance_pos).0),
        max_val,
    );
    Ok(overrides)
}

/// Poll the solver health endpoint for up to 30 seconds, giving it time to become healthy.
async fn wait_for_health(client: &FyndClient, fynd_url: &str) -> anyhow::Result<HealthStatus> {
    info!("Checking solver health at {fynd_url}...");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        match client.health().await {
            Ok(h) if h.healthy() => return Ok(h),
            Ok(h) => {
                if tokio::time::Instant::now() >= deadline {
                    bail!(
                        "solver at {fynd_url} not healthy after 30s \
                         (last update: {}ms ago, {} pools); \
                         wait for market data to load",
                        h.last_update_ms(),
                        h.num_solver_pools()
                    );
                }
                info!(
                    "Solver not ready yet ({} pools, last update {}ms ago), retrying...",
                    h.num_solver_pools(),
                    h.last_update_ms()
                );
            }
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    bail!("health check failed after 30s: {e}");
                }
                info!("Health check failed ({e}), retrying...");
            }
        }
    }
}

/// Submit an ERC-20 approval, optionally skipping or customising the allowance check.
async fn ensure_approval(
    client: &FyndClient,
    signer: &PrivateKeySigner,
    sell_token: Bytes,
    amount: BigUint,
    allowance_check: AllowanceCheck,
    transfer_type: UserTransferType,
    sender: Address,
) -> anyhow::Result<()> {
    println!("Checking ERC-20 allowance...");
    let params =
        ApprovalParams::new(sell_token, amount, allowance_check).with_transfer_type(transfer_type);
    let hints = SigningHints::default().with_sender(sender);
    let Some(approval_payload) = client.approval(&params, &hints).await? else {
        println!("Allowance sufficient, no approval needed.");
        return Ok(());
    };
    println!("Allowance insufficient — submitting approval transaction...");
    let sig = signer
        .sign_hash(&approval_payload.signing_hash())
        .await?;
    let signed = SignedApproval::assemble(approval_payload, sig);
    let receipt = client.execute_approval(signed).await?;
    let mined = tokio::time::timeout(Duration::from_secs(120), receipt)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for approval to be mined"))??;
    println!("Approved! (tx: {:#x})", mined.tx_hash());
    Ok(())
}

struct Permit2Args {
    sell_token: Address,
    sender: Address,
    permit2_addr: Address,
    router_addr: Address,
    slippage: f64,
    execute: bool,
}

/// Build Permit2 `EncodingOptions`: read nonce, sign EIP-712 hash.
async fn create_permit2_encoding(
    provider: &RootProvider<Ethereum>,
    signer: &PrivateKeySigner,
    args: Permit2Args,
) -> anyhow::Result<EncodingOptions> {
    let (nonce, expiration, sig_deadline) = if args.execute {
        let nonce = permit2::read_nonce(
            provider,
            args.permit2_addr,
            args.sender,
            args.sell_token,
            args.router_addr,
        )
        .await?;
        info!("Permit2 nonce for sender: {nonce}");

        let block = provider
            .get_block_by_number(Default::default())
            .await?
            .ok_or_else(|| anyhow::anyhow!("could not fetch latest block"))?;
        let now = block.header.timestamp;
        (nonce, now + 3_600, now + 1_800) // expiration: +1h, sig_deadline: +30m
    } else {
        // Dry-run: ephemeral key, nonce is 0, use max uint48 deadlines.
        (0u64, 281_474_976_710_655u64, 281_474_976_710_655u64)
    };

    let fynd_permit = FyndPermitSingle::new(
        FyndPermitDetails::new(
            Bytes::copy_from_slice(args.sell_token.as_slice()),
            max_uint160(),
            BigUint::from(expiration),
            BigUint::from(nonce),
        ),
        Bytes::copy_from_slice(args.router_addr.as_slice()),
        BigUint::from(sig_deadline),
    );

    let chain_id = provider.get_chain_id().await?;
    info!("Signing Permit2 EIP-712 hash (chain_id={chain_id}, nonce={nonce})...");
    let permit2_bytes = Bytes::copy_from_slice(args.permit2_addr.as_slice());
    let signing_hash = fynd_permit.eip712_signing_hash(chain_id, &permit2_bytes)?;
    let sig = signer
        .sign_hash(&B256::from(signing_hash))
        .await?;
    let signature = Bytes::copy_from_slice(&sig.as_bytes());

    Ok(EncodingOptions::new(args.slippage).with_permit2(fynd_permit, signature)?)
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Parse token addresses
    let sell_token = Address::from_str(&cli.sell_token)
        .with_context(|| format!("invalid sell token: {}", cli.sell_token))?;
    let buy_token = Address::from_str(&cli.buy_token)
        .with_context(|| format!("invalid buy token: {}", cli.buy_token))?;
    let permit2_addr = Address::from_str(&cli.permit2)
        .with_context(|| format!("invalid permit2 address: {}", cli.permit2))?;

    // Load or generate signer
    let signer = if cli.execute {
        let pk_hex = env::var("PRIVATE_KEY")
            .map_err(|_| anyhow::anyhow!("--execute requires PRIVATE_KEY environment variable"))?;
        let pk_bytes =
            B256::from_str(&pk_hex).map_err(|e| anyhow::anyhow!("invalid PRIVATE_KEY: {e}"))?;
        PrivateKeySigner::from_bytes(&pk_bytes)
            .map_err(|e| anyhow::anyhow!("invalid PRIVATE_KEY: {e}"))?
    } else {
        PrivateKeySigner::random()
    };
    let sender = signer.address();
    info!("Sender: {sender:?}");

    let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(
        cli.rpc_url
            .parse::<reqwest::Url>()
            .with_context(|| format!("invalid RPC URL: {}", cli.rpc_url))?,
    );

    // ── Build FyndClient ──────────────────────────────────────────────────────
    let client = FyndClientBuilder::new(&cli.fynd_url, &cli.rpc_url)
        .with_sender(sender)
        .build()
        .await?;

    // ── Health check ──────────────────────────────────────────────────────────
    let health = wait_for_health(&client, &cli.fynd_url).await?;
    info!(
        "Solver healthy: {}, last update: {}ms ago, {} solver pools",
        health.healthy(),
        health.last_update_ms(),
        health.num_solver_pools(),
    );

    // ── Shared order fields ───────────────────────────────────────────────────
    let sell_token_bytes = Bytes::copy_from_slice(sell_token.as_slice());
    let buy_token_bytes = Bytes::copy_from_slice(buy_token.as_slice());
    let sender_bytes = Bytes::copy_from_slice(sender.as_slice());
    let amount = BigUint::from(cli.sell_amount);
    let slippage = cli.slippage_bps as f64 / 10_000.0;

    // ── Setup: encoding options, approvals, dry-run spenders ─────────────────
    // dry_run_spenders: token spenders whose allowance slots are injected for dry-run simulation.
    let (encoding_options, dry_run_spenders): (_, Vec<Address>) = match cli.transfer_type {
        TransferType::TransferFrom => {
            if cli.execute {
                ensure_approval(
                    &client,
                    &signer,
                    sell_token_bytes.clone(),
                    amount.clone(),
                    AllowanceCheck::AtLeast(amount.clone()),
                    UserTransferType::TransferFrom,
                    sender,
                )
                .await?;
            }
            let info = client.info().await?;
            let router_addr = Address::try_from(info.router_address().as_ref())
                .map_err(|_| anyhow::anyhow!("invalid router address from /v1/info"))?;
            (EncodingOptions::new(slippage), vec![router_addr])
        }
        TransferType::TransferFromPermit2 => {
            let info = client.info().await?;
            let router_addr = Address::try_from(info.router_address().as_ref())
                .map_err(|_| anyhow::anyhow!("invalid router address from /v1/info"))?;
            if cli.execute {
                // Check against the swap amount but approve max — subsequent swaps won't
                // need re-approval even after Permit2 deducts from the ERC-20 allowance.
                ensure_approval(
                    &client,
                    &signer,
                    sell_token_bytes.clone(),
                    max_uint160(),
                    AllowanceCheck::AtLeast(amount.clone()),
                    UserTransferType::TransferFromPermit2,
                    sender,
                )
                .await?;
            }
            let enc = create_permit2_encoding(
                &provider,
                &signer,
                Permit2Args {
                    sell_token,
                    sender,
                    permit2_addr,
                    router_addr,
                    slippage,
                    execute: cli.execute,
                },
            )
            .await?;
            (enc, vec![permit2_addr, router_addr])
        }
        TransferType::UseVaultsFunds => (EncodingOptions::new(slippage).with_vault_funds(), vec![]),
    };

    // ── Quote ─────────────────────────────────────────────────────────────────
    info!(
        "Requesting quote: {} atomic units of {} -> {}",
        cli.sell_amount, cli.sell_token, cli.buy_token
    );
    let order = Order::new(
        sell_token_bytes.clone(),
        buy_token_bytes.clone(),
        amount.clone(),
        OrderSide::Sell,
        sender_bytes,
        None,
    );
    let quote_options = QuoteOptions::default()
        .with_timeout_ms(5_000)
        .with_encoding_options(encoding_options);
    let quote = client
        .quote(QuoteParams::new(order, quote_options))
        .await?;

    println!("\n========== Quote ==========");
    println!("Status:              {:?}", quote.status());
    println!("Amount in:           {}", quote.amount_in());
    println!("Amount out:          {}", quote.amount_out());
    println!("Amount out net gas:  {}", quote.amount_out_net_gas());
    println!("Token in:            {}", format!("0x{}", hex::encode(&sell_token_bytes)));
    println!("Token out:           {}", format!("0x{}", hex::encode(&buy_token_bytes)));
    println!("Gas estimate:        {}", quote.gas_estimate());
    println!("Solve time:          {}ms", quote.solve_time_ms());
    if let Some(route) = quote.route() {
        println!("Route ({} hops):", route.swaps().len());
        for (i, swap) in route.swaps().iter().enumerate() {
            println!(
                "  {}. 0x{} -> 0x{} via {} (pool: {})",
                i + 1,
                hex::encode(swap.token_in()),
                hex::encode(swap.token_out()),
                swap.protocol(),
                swap.component_id(),
            );
        }
    }
    println!("============================\n");

    // ── Sign order payload ────────────────────────────────────────────────────
    let gas_limit: u64 = quote.gas_estimate().try_into().unwrap();
    // For dry-run, set gas price to 0 so eth_call bypasses the ETH balance check.
    // The EVM still executes the full contract code; it just skips gas*price deduction.
    let signing_hints = if cli.execute {
        SigningHints::default().with_gas_limit(gas_limit * 10u64)
    } else {
        SigningHints::default()
            .with_gas_limit(gas_limit * 10u64)
            .with_max_fee_per_gas(0)
            .with_max_priority_fee_per_gas(0)
    };
    let payload = client
        .swap_payload(quote, &signing_hints)
        .await?;
    let order_sig = signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let signed = SignedSwap::assemble(payload, order_sig);

    // ── Build execution options ───────────────────────────────────────────────
    let exec_options = if cli.execute {
        println!("Submitting on-chain transaction...");
        ExecutionOptions::default()
    } else {
        let overrides = if dry_run_spenders.is_empty() {
            None
        } else {
            let mut overrides =
                build_dry_run_overrides(&provider, sell_token, sender, dry_run_spenders[0]).await?;
            for &spender in &dry_run_spenders[1..] {
                let extra = build_dry_run_overrides(&provider, sell_token, sender, spender).await?;
                overrides.merge(extra);
            }
            Some(overrides)
        };
        info!("Submitting dry-run execution...");
        ExecutionOptions { dry_run: true, storage_overrides: overrides, fetch_revert_reason: true }
    };

    // ── Execute ───────────────────────────────────────────────────────────────
    let receipt = client
        .execute_swap(signed, &exec_options)
        .await?;
    let settled = if cli.execute {
        tokio::time::timeout(Duration::from_secs(120), receipt)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for transaction to be mined"))??
    } else {
        receipt.await?
    };

    if cli.execute {
        if let Some(hash) = settled.tx_hash() {
            println!("\nSwap executed on-chain! (tx: {hash:#x})");
        } else {
            println!("\nSwap executed on-chain!");
        }
    } else {
        println!("\nSimulation successful!");
    }
    match settled.settled_amount() {
        Some(a) => println!("Settled amount: {a}"),
        None => println!("Settled amount: (no matching Transfer log)"),
    }
    println!("Gas cost (wei): {}", settled.gas_cost());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- max_uint160 -----------------------------------------------------------

    #[test]
    fn max_uint160_is_20_bytes_of_ff() {
        let val = max_uint160();
        assert_eq!(val.to_bytes_be(), vec![0xFFu8; 20]);
    }

    #[test]
    fn max_uint160_equals_two_pow_160_minus_one() {
        use num_bigint::BigUint;
        let expected = (BigUint::from(1u8) << 160u32) - BigUint::from(1u8);
        assert_eq!(max_uint160(), expected);
    }

    // -- CLI parsing -----------------------------------------------------------

    #[test]
    fn cli_defaults() {
        std::env::remove_var("RPC_URL");
        let cli = Cli::try_parse_from(["fynd-swap-cli"]).unwrap();
        assert_eq!(cli.sell_token, "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert_eq!(cli.buy_token, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        assert_eq!(cli.sell_amount, 1_000_000_000_000_000_000u128);
        assert_eq!(cli.slippage_bps, 50u32);
        assert_eq!(cli.fynd_url, "http://localhost:3000");
        assert_eq!(cli.transfer_type, TransferType::TransferFrom);
        assert_eq!(cli.permit2, "0x000000000022D473030F116dDEE9F6B43aC78BA3");
        assert!(!cli.execute);
    }

    #[test]
    fn transfer_type_transfer_from_parses() {
        let cli =
            Cli::try_parse_from(["fynd-swap-cli", "--transfer-type", "transfer-from"]).unwrap();
        assert_eq!(cli.transfer_type, TransferType::TransferFrom);
    }

    #[test]
    fn transfer_type_transfer_from_permit2_parses() {
        let cli =
            Cli::try_parse_from(["fynd-swap-cli", "--transfer-type", "transfer-from-permit2"])
                .unwrap();
        assert_eq!(cli.transfer_type, TransferType::TransferFromPermit2);
    }

    #[test]
    fn transfer_type_unknown_is_rejected() {
        assert!(Cli::try_parse_from(["fynd-swap-cli", "--transfer-type", "banana"]).is_err());
    }
}
