//! fynd-swap-cli — quote and execute token swaps via the Fynd solver.
//!
//! Supports both ERC-20 approve and Permit2 transfer flows. When `--tycho-url` is
//! provided, an embedded Fynd solver is spawned automatically instead of connecting
//! to an external instance.
//!
//! # Dry-run (default)
//!
//! Uses an ephemeral key and ERC-20 storage overrides so no funds are required.
//!
//! # On-chain execution (`--execute`)
//!
//! Requires `PRIVATE_KEY` env var and a funded wallet with appropriate approvals.

use std::{env, str::FromStr, time::Duration};

use actix_web::dev::ServerHandle;
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
    EncodingOptions, ExecutionOptions, FyndClient, FyndClientBuilder, HealthStatus, Order,
    OrderSide, PermitDetails as FyndPermitDetails, PermitSingle as FyndPermitSingle, QuoteOptions,
    QuoteParams, SignedOrder, SigningHints, StorageOverrides,
};
use fynd_rpc::{
    builder::{parse_chain, FyndRPCBuilder},
    config::WorkerPoolsConfig,
    protocols::fetch_protocol_systems,
};
use num_bigint::BigUint;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tycho_simulation::tycho_common::models::Chain;

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
}

/// fynd-swap-cli — quote and execute token swaps via the Fynd solver.
#[derive(Parser)]
#[command(name = "fynd-swap-cli")]
#[command(about = "Quote and execute token swaps via Fynd (ERC-20 or Permit2)")]
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

    /// Slippage tolerance in basis points (e.g. 50 = 0.5%)
    #[arg(long, default_value_t = 50u32)]
    slippage_bps: u32,

    /// Fynd solver URL (ignored when --tycho-url is set)
    #[arg(long, default_value = "http://localhost:3000")]
    fynd_url: String,

    /// Token transfer flow
    #[arg(long, default_value = "transfer-from")]
    transfer_type: TransferType,

    /// Submit the swap on-chain instead of dry-running it.
    /// Requires the PRIVATE_KEY environment variable.
    #[arg(long)]
    execute: bool,

    /// Tycho Router address (required for transfer-from-permit2 flow)
    #[arg(long)]
    router: Option<String>,

    /// Permit2 contract address (defaults to the canonical cross-chain deployment)
    #[arg(long, default_value = "0x000000000022D473030F116dDEE9F6B43aC78BA3")]
    permit2: String,

    /// If set, spawn an embedded Fynd solver connecting to this Tycho WebSocket URL
    #[arg(long)]
    tycho_url: Option<String>,

    /// Tycho API key
    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: Option<String>,

    /// Disable TLS for the Tycho WebSocket connection
    #[arg(long)]
    disable_tls: bool,

    /// Node RPC URL for the target chain
    #[arg(long, env = "RPC_URL", default_value = "https://eth.llamarpc.com")]
    rpc_url: String,

    /// Target chain (e.g. Ethereum)
    #[arg(long, default_value = "Ethereum")]
    chain: String,

    /// Protocols to index (comma-separated). Only used with --tycho-url.
    /// If empty, all on-chain protocols are fetched from Tycho.
    #[arg(long, value_delimiter = ',')]
    protocols: Vec<String>,

    /// Path to worker pools TOML config. Only used with --tycho-url.
    /// If absent, uses a sensible default (most_liquid, 1-3 hops).
    #[arg(long)]
    worker_pools_config: Option<String>,

    /// HTTP port for the embedded solver
    #[arg(long, default_value_t = 3000u16)]
    http_port: u16,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Max uint160 — used as the Permit2 approved amount (unlimited).
fn max_uint160() -> BigUint {
    BigUint::from_bytes_be(&[0xFF; 20])
}

/// Detect ERC-20 storage slots and build `StorageOverrides` for a dry-run.
///
/// Injects `U256::MAX` into both the holder's balance slot and the
/// `holder → spender` allowance slot so the simulation succeeds without real funds.
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

    let max_val = Bytes::copy_from_slice(&B256::from(U256::MAX).0);
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

struct EmbeddedSolverConfig<'a> {
    tycho_url: &'a str,
    tycho_api_key: Option<&'a str>,
    disable_tls: bool,
    rpc_url: &'a str,
    chain: Chain,
    protocols: Vec<String>,
    worker_pools_config: Option<&'a str>,
    http_port: u16,
}

/// Spawn an embedded Fynd solver and return its server handle for later shutdown.
async fn spawn_embedded_solver(cfg: EmbeddedSolverConfig<'_>) -> anyhow::Result<ServerHandle> {
    let pools_config = match cfg.worker_pools_config {
        Some(path) => WorkerPoolsConfig::load_from_file(path)?,
        None => toml::from_str(
            r#"
[pools.default]
algorithm = "most_liquid"
min_hops = 1
max_hops = 3
"#,
        )
        .context("failed to parse default worker pools config")?,
    };

    let resolved_protocols = if cfg.protocols.is_empty() {
        let fetched =
            fetch_protocol_systems(cfg.tycho_url, cfg.tycho_api_key, !cfg.disable_tls, cfg.chain)
                .await?;
        if fetched.is_empty() {
            bail!("no protocols found; check Tycho connectivity or use --protocols");
        }
        fetched
    } else {
        cfg.protocols
    };

    info!("Starting embedded solver with {} protocol(s)", resolved_protocols.len());

    let mut builder = FyndRPCBuilder::new(
        cfg.chain,
        pools_config.pools,
        cfg.tycho_url.to_string(),
        cfg.rpc_url.to_string(),
        resolved_protocols,
    )
    .http_port(cfg.http_port);

    if cfg.disable_tls {
        builder = builder.disable_tls();
    }
    if let Some(api_key) = cfg.tycho_api_key {
        builder = builder.tycho_api_key(api_key.to_string());
    }

    let fynd = builder
        .build()
        .context("failed to build embedded solver")?;
    let handle = fynd.server_handle();
    tokio::spawn(async move { fynd.run().await.ok() });
    Ok(handle)
}

/// Poll the solver health endpoint until healthy or deadline expires.
///
/// For embedded solvers, retries for up to 60 seconds. For external solvers,
/// checks once and fails immediately if not healthy.
async fn wait_for_health(
    client: &FyndClient,
    fynd_url: &str,
    is_embedded: bool,
) -> anyhow::Result<HealthStatus> {
    info!("Checking solver health at {fynd_url}...");
    let health_deadline = tokio::time::Instant::now() +
        if is_embedded { Duration::from_secs(60) } else { Duration::ZERO };

    loop {
        match client.health().await {
            Ok(h) if h.healthy() => break Ok(h),
            other => {
                if is_embedded && tokio::time::Instant::now() < health_deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                match other {
                    Ok(h) => bail!(
                        "solver at {fynd_url} is not healthy \
                         (last update: {}ms ago, {} pools); \
                         wait for market data to load",
                        h.last_update_ms(),
                        h.num_solver_pools()
                    ),
                    Err(e) if is_embedded => {
                        bail!("embedded solver did not become healthy within 60s: {e}")
                    }
                    Err(e) => bail!("health check failed: {e}"),
                }
            }
        }
    }
}

struct Permit2Args<'a> {
    sell_token: Address,
    sender: Address,
    permit2_addr: Address,
    router_str: &'a str,
    amount: &'a BigUint,
    slippage: f64,
    execute: bool,
}

/// Build Permit2 `EncodingOptions`: validate allowance, read nonce, sign EIP-712 hash.
async fn create_permit2_encoding(
    provider: &RootProvider<Ethereum>,
    signer: &PrivateKeySigner,
    args: Permit2Args<'_>,
) -> anyhow::Result<EncodingOptions> {
    let router_addr = Address::from_str(args.router_str)
        .with_context(|| format!("invalid router address: {}", args.router_str))?;

    let (nonce, expiration, sig_deadline) = if args.execute {
        let allowance =
            erc20::read_erc20_allowance(provider, args.sell_token, args.sender, args.permit2_addr)
                .await?;
        if allowance < *args.amount {
            eprintln!("\nError: insufficient ERC-20 allowance to the Permit2 contract.");
            eprintln!("  Token:     {:#x}", args.sell_token);
            eprintln!("  Permit2:   {:#x}", args.permit2_addr);
            eprintln!("  Allowance: {allowance}");
            eprintln!("  Required:  {}", args.amount);
            eprintln!("\nApprove Permit2 with:");
            eprintln!(
                "  cast send {:#x} \"approve(address,uint256)\" \
                 {:#x} {} \\\n    \
                 --rpc-url $RPC_URL --private-key $PRIVATE_KEY",
                args.sell_token,
                args.permit2_addr,
                u128::MAX
            );
            bail!("insufficient allowance to Permit2");
        }

        let nonce = permit2::read_nonce(
            provider,
            args.permit2_addr,
            args.sender,
            args.sell_token,
            router_addr,
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
        Bytes::copy_from_slice(router_addr.as_slice()),
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

    // ── Optionally spawn an embedded solver ───────────────────────────────────
    let (fynd_url, server_handle): (String, Option<ServerHandle>) =
        if let Some(ref tycho_url) = cli.tycho_url {
            let chain =
                parse_chain(&cli.chain).with_context(|| format!("invalid chain: {}", cli.chain))?;
            let handle = spawn_embedded_solver(EmbeddedSolverConfig {
                tycho_url,
                tycho_api_key: cli.tycho_api_key.as_deref(),
                disable_tls: cli.disable_tls,
                rpc_url: &cli.rpc_url,
                chain,
                protocols: cli.protocols.clone(),
                worker_pools_config: cli.worker_pools_config.as_deref(),
                http_port: cli.http_port,
            })
            .await?;
            (format!("http://localhost:{}", cli.http_port), Some(handle))
        } else {
            (cli.fynd_url.clone(), None)
        };

    // ── Build FyndClient ──────────────────────────────────────────────────────
    let client = FyndClientBuilder::new(&fynd_url, &cli.rpc_url)
        .with_sender(sender)
        .build()
        .await?;

    // ── Health check (polls for embedded solver; checks once for external) ────
    let is_embedded = server_handle.is_some();
    let health = wait_for_health(&client, &fynd_url, is_embedded).await?;
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

    // ── Permit2 pre-flight ────────────────────────────────────────────────────
    let encoding_options = match cli.transfer_type {
        TransferType::TransferFrom => EncodingOptions::new(slippage),
        TransferType::TransferFromPermit2 => {
            let router_str = cli.router.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--router is required for --transfer-type transfer-from-permit2")
            })?;
            create_permit2_encoding(
                &provider,
                &signer,
                Permit2Args {
                    sell_token,
                    sender,
                    permit2_addr,
                    router_str,
                    amount: &amount,
                    slippage,
                    execute: cli.execute,
                },
            )
            .await?
        }
    };

    // ── Quote ─────────────────────────────────────────────────────────────────
    info!(
        "Requesting quote: {} atomic units of {} -> {}",
        cli.sell_amount, cli.sell_token, cli.buy_token
    );
    let order = Order::new(
        sell_token_bytes,
        buy_token_bytes,
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
    println!("Status:       {:?}", quote.status());
    println!("Amount in:    {}", quote.amount_in());
    println!("Amount out:   {}", quote.amount_out());
    println!("Gas estimate: {}", quote.gas_estimate());
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

    let tx = quote.transaction().ok_or_else(|| {
        anyhow::anyhow!("quote has no calldata; ensure encoding_options were set in the request")
    })?;
    let router_from_quote = Address::from_slice(tx.to().as_ref());

    // ── ERC-20 execute: verify allowance before signing ───────────────────────
    if cli.execute {
        if let TransferType::TransferFrom = cli.transfer_type {
            let allowance =
                erc20::read_erc20_allowance(&provider, sell_token, sender, router_from_quote)
                    .await?;
            if allowance < amount {
                eprintln!("\nError: insufficient sell-token allowance for the Fynd router.");
                eprintln!("  Token:     {sell_token:#x}");
                eprintln!("  Router:    {router_from_quote:#x}");
                eprintln!("  Allowance: {allowance}");
                eprintln!("  Required:  {amount}");
                eprintln!("\nApprove the router with:");
                eprintln!(
                    "  cast send {sell_token:#x} \"approve(address,uint256)\" \
                     {router_from_quote:#x} {} \\\n    \
                     --rpc-url $RPC_URL --private-key $PRIVATE_KEY",
                    cli.sell_amount
                );
                bail!("insufficient sell-token allowance");
            }
        }
    }

    // ── Sign order payload ────────────────────────────────────────────────────
    let payload = client
        .signable_payload(quote, &SigningHints::default())
        .await?;
    let order_sig = signer
        .sign_hash(&payload.signing_hash())
        .await?;
    let signed = SignedOrder::assemble(payload, order_sig);

    // ── Build execution options ───────────────────────────────────────────────
    let exec_options = if cli.execute {
        info!("Submitting on-chain transaction...");
        ExecutionOptions::default()
    } else {
        // Dry-run: the spender for the allowance slot depends on the flow.
        // TransferFrom: spender is the Fynd router (from quote).
        // TransferFromPermit2: spender is the Permit2 contract (router authorised via EIP-712).
        let spender = match cli.transfer_type {
            TransferType::TransferFrom => router_from_quote,
            TransferType::TransferFromPermit2 => permit2_addr,
        };
        let overrides = build_dry_run_overrides(&provider, sell_token, sender, spender).await?;
        info!("Submitting dry-run execution...");
        ExecutionOptions { dry_run: true, storage_overrides: Some(overrides) }
    };

    // ── Execute ───────────────────────────────────────────────────────────────
    let receipt = client
        .execute(signed, &exec_options)
        .await?;
    let settled = if cli.execute {
        tokio::time::timeout(Duration::from_secs(120), receipt)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for transaction to be mined"))??
    } else {
        receipt.await?
    };

    if cli.execute {
        println!("\nSwap executed on-chain!");
    } else {
        println!("\nSimulation successful!");
    }
    match settled.settled_amount() {
        Some(a) => println!("Settled amount: {a}"),
        None => println!("Settled amount: (no matching Transfer log)"),
    }
    println!("Gas cost (wei): {}", settled.gas_cost());

    // ── Shutdown embedded solver ──────────────────────────────────────────────
    if let Some(handle) = server_handle {
        info!("Stopping embedded solver...");
        handle.stop(true).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── max_uint160 ───────────────────────────────────────────────────────────

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

    // ── CLI parsing ───────────────────────────────────────────────────────────

    #[test]
    fn cli_defaults() {
        std::env::remove_var("TYCHO_API_KEY");
        std::env::remove_var("RPC_URL");
        let cli = Cli::try_parse_from(["fynd-swap-cli"]).unwrap();
        assert_eq!(cli.sell_token, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        assert_eq!(cli.buy_token, "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert_eq!(cli.sell_amount, 1_000_000_000u128);
        assert_eq!(cli.slippage_bps, 50u32);
        assert_eq!(cli.fynd_url, "http://localhost:3000");
        assert_eq!(cli.transfer_type, TransferType::TransferFrom);
        assert_eq!(cli.permit2, "0x000000000022D473030F116dDEE9F6B43aC78BA3");
        assert_eq!(cli.http_port, 3000u16);
        assert!(!cli.execute);
        assert!(cli.tycho_url.is_none());
        assert!(cli.router.is_none());
        assert!(cli.protocols.is_empty());
    }

    #[test]
    fn cli_permit2_transfer_type_with_router() {
        let cli = Cli::try_parse_from([
            "fynd-swap-cli",
            "--transfer-type",
            "transfer-from-permit2",
            "--router",
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        ])
        .unwrap();
        assert_eq!(cli.transfer_type, TransferType::TransferFromPermit2);
        assert_eq!(cli.router.as_deref(), Some("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"));
    }

    #[test]
    fn cli_tycho_url_sets_embedded_solver_path() {
        let cli = Cli::try_parse_from(["fynd-swap-cli", "--tycho-url", "localhost:8888"]).unwrap();
        assert_eq!(cli.tycho_url.as_deref(), Some("localhost:8888"));
        // fynd_url keeps its default; the embedded solver overrides the URL at runtime
        assert_eq!(cli.fynd_url, "http://localhost:3000");
    }

    #[test]
    fn cli_protocols_split_by_comma() {
        let cli =
            Cli::try_parse_from(["fynd-swap-cli", "--protocols", "uniswap_v2,uniswap_v3,curve"])
                .unwrap();
        assert_eq!(cli.protocols, vec!["uniswap_v2", "uniswap_v3", "curve"]);
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
