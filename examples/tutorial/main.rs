//! Tutorial Example: Quote, Simulate & Execute Swaps
//!
//! This example demonstrates how to:
//! 1. Call an already-running tycho-router solver for a swap quote
//! 2. Display the route and pricing
//! 3. Encode the swap via tycho-execution
//! 4. Optionally simulate (eth_simulate or Tenderly) or execute on-chain

mod types;

use std::{collections::HashMap, env, str::FromStr};

use alloy::{
    network::{Ethereum, EthereumWallet},
    primitives::{Address, Bytes as AlloyBytes, Keccak256, Signature, TxKind, B256, U256},
    providers::{
        fillers::{FillProvider, JoinFill, WalletFiller},
        Identity, Provider, ProviderBuilder, RootProvider,
    },
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        TransactionInput, TransactionRequest,
    },
    signers::{local::PrivateKeySigner, SignerSync},
    sol_types::{eip712_domain, SolStruct, SolValue},
};
use alloy_chains::NamedChain;
use clap::Parser;
use dialoguer::{theme::ColorfulTheme, Select};
use fynd_core::{Order, OrderSide, Route, Solution, SolutionOptions, SolutionRequest};
use fynd_rpc::{builder::parse_chain, HealthStatus};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tycho_execution::encoding::{
    evm::{
        approvals::permit2::PermitSingle, encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::{EncodedSolution, Solution as ExecutionSolution, Transaction, UserTransferType},
};
use tycho_simulation::{
    evm::protocol::u256_num::biguint_to_u256,
    tycho_client::rpc::{HttpRPCClient, HttpRPCClientOptions, RPCClient},
    tycho_common::{
        dto::{PaginationParams, ProtocolComponentsRequestBody},
        models::{protocol::ProtocolComponent, token::Token, Chain},
        Bytes,
    },
    utils::load_all_tokens,
};
// Import tutorial-specific types
use types::{
    SwapToExecution, TenderlySimulation, TenderlySimulationRequest, TenderlySimulationResponse,
};

/// Tutorial CLI: Quote, simulate, and execute swaps via tycho-router
#[derive(Parser)]
#[command(name = "tutorial")]
#[command(about = "Get quotes from tycho-router and optionally simulate/execute swaps")]
struct Cli {
    /// Sell token address (defaults to USDC on the mainnet)
    #[arg(long, default_value = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    sell_token: String,

    /// Buy token address (defaults to WETH on the mainnet)
    #[arg(long, default_value = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    buy_token: String,

    /// Amount to sell (in decimal units, e.g., 1000.0 for 10 USDC)
    #[arg(long, default_value_t = 1000.0)]
    sell_amount: f64,

    /// Blockchain network
    #[arg(long, default_value = "ethereum")]
    chain: String,

    /// Solver API URL
    #[arg(long, default_value = "http://localhost:3000")]
    solver_url: String,

    /// Minimum TVL threshold for pools (denominated in ETH)
    #[arg(long, default_value_t = 10.0)]
    tvl_threshold: f64,

    /// Only simulate, don't prompt for execution
    #[arg(long)]
    simulate_only: bool,

    /// Use Tenderly for simulation instead of eth_simulate
    #[arg(long)]
    use_tenderly: bool,

    /// Slippage tolerance in basis points (default: 50 = 0.5%)
    #[arg(long, default_value_t = 50)]
    slippage_bps: u32,

    /// Protocol systems to use (comma-separated, e.g., "uniswap_v2,uniswap_v3").
    /// If not specified, all available protocol systems will be fetched from the API.
    #[arg(long)]
    protocols: Option<String>,

    /// Sender address for simulation (use with --simulate-only to avoid exposing private key).
    /// If the sender lacks funds/approvals, the simulation will fail as expected.
    #[arg(long)]
    sender: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env file
    dotenv::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let chain = parse_chain(&cli.chain)?;

    let solver_url = cli.solver_url.clone();
    let tycho_url =
        env::var("TYCHO_URL").unwrap_or_else(|_| "tycho-beta.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").ok();
    let rpc_url = env::var("RPC_URL").ok();
    let private_key = env::var("PRIVATE_KEY").ok();

    // Create tycho-client RPC client
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(tycho_api_key.clone());

    let tycho_rpc = HttpRPCClient::new(&format!("https://{}", tycho_url), rpc_options)
        .map_err(|e| format!("Failed to create Tycho RPC client: {}", e))?;

    // Determine protocol systems to use
    let protocol_systems = match &cli.protocols {
        Some(protocols) => {
            // User specified protocols
            protocols
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        }
        None => {
            // Fetch all available protocol systems from the API
            info!("No protocols specified, fetching all available protocol systems...");
            fetch_protocol_systems(&tycho_rpc, chain).await?
        }
    };
    info!("Using {} protocol systems", protocol_systems.len());

    // Check solver health
    info!("Checking solver health at {}...", solver_url);
    let client = reqwest::Client::new();
    let health = check_solver_health(&client, &solver_url).await?;
    info!(
        "Solver healthy: {}, last update: {}ms ago, {} solver pools",
        health.healthy, health.last_update_ms, health.num_solver_pools
    );

    if !health.healthy {
        return Err("Solver is not healthy. Please wait for market data to load.".into());
    }

    // Load tokens from Tycho indexer
    info!("Loading tokens from Tycho indexer...");
    let all_tokens = load_all_tokens(
        &tycho_url,
        false, // use_http
        tycho_api_key.as_deref(),
        true, // include_all
        chain,
        None, // quality filter
        None, // address filter
    )
    .await?;
    info!("Loaded {} tokens", all_tokens.len());

    // Resolve sell and buy tokens
    let sell_token_address = Bytes::from_str(&cli.sell_token)?;
    let buy_token_address = Bytes::from_str(&cli.buy_token)?;

    let sell_token = all_tokens
        .get(&sell_token_address)
        .ok_or_else(|| format!("Sell token not found: {}", cli.sell_token))?
        .clone();
    let buy_token = all_tokens
        .get(&buy_token_address)
        .ok_or_else(|| format!("Buy token not found: {}", cli.buy_token))?
        .clone();

    // Calculate amount in base units
    let amount_in =
        BigUint::from((cli.sell_amount * 10f64.powi(sell_token.decimals as i32)) as u128);

    info!("Getting quote: {} {} -> {}", cli.sell_amount, sell_token.symbol, buy_token.symbol);

    // Fetch protocol components via tycho-client
    info!("Fetching protocol components via Tycho API...");
    let components =
        fetch_amm_components(&tycho_rpc, chain, &protocol_systems, cli.tvl_threshold).await?;
    info!("Fetched {} protocol components", components.len());

    // Determine user address for the quote
    // Priority: PRIVATE_KEY > --sender > zero address
    let user_address = if let Some(ref pk) = private_key {
        let pk_bytes = B256::from_str(pk)?;
        let signer = PrivateKeySigner::from_bytes(&pk_bytes)?;
        format!("{:?}", signer.address())
    } else if let Some(ref sender) = cli.sender {
        sender.clone()
    } else {
        "0x0000000000000000000000000000000000000000".to_string()
    };

    // Call solver API
    let quote = get_solver_quote(
        &client,
        &solver_url,
        &sell_token_address,
        &buy_token_address,
        &amount_in,
        &user_address,
    )
    .await?;

    // Display quote
    display_quote(&quote, &sell_token, &buy_token, &amount_in, &all_tokens)?;

    // Determine if we can proceed with simulation/execution
    let can_simulate_only = cli.simulate_only && cli.sender.is_some();
    if private_key.is_none() && !can_simulate_only {
        return Err("No PRIVATE_KEY set. Set it to simulate or execute swaps, \
            or use --simulate-only --sender <address> to simulate without a private key."
            .into());
    }

    let rpc_url =
        rpc_url.ok_or("RPC_URL environment variable required for simulation/execution")?;

    // Get the route from the quote
    let order_solution = quote
        .orders
        .first()
        .ok_or("No order solution")?;
    let route = order_solution
        .route
        .as_ref()
        .ok_or("No route in solution")?;

    // Determine user address and transfer type based on available credentials
    let (simulation_address, transfer_type, signer) = if let Some(ref pk_str) = private_key {
        let pk_bytes = B256::from_str(pk_str)?;
        let signer = PrivateKeySigner::from_bytes(&pk_bytes)?;
        (signer.address(), UserTransferType::TransferFromPermit2, Some(signer))
    } else {
        // --simulate-only with --sender (no private key)
        let sender_str = cli.sender.as_ref().unwrap();
        let sender_addr = Address::from_str(sender_str)?;
        println!("\nSimulating as sender {} (no private key - using TransferFrom)", sender_str);
        (sender_addr, UserTransferType::TransferFrom, None)
    };

    // Map solver route to execution solution
    let execution_solution = map_route_to_execution_solution(
        route,
        &components,
        &sell_token,
        &buy_token,
        &amount_in,
        Bytes::from(simulation_address.to_vec()),
        cli.slippage_bps,
    )?;

    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .expect("Failed to get default SwapEncoderRegistry");

    // Encode via TychoRouterEncoderBuilder
    let encoder = TychoRouterEncoderBuilder::new()
        .chain(chain)
        .user_transfer_type(transfer_type.clone())
        .swap_encoder_registry(swap_encoder_registry)
        .build()?;

    let encoded_solutions = encoder.encode_solutions(vec![execution_solution.clone()])?;
    let encoded_solution = encoded_solutions
        .into_iter()
        .next()
        .ok_or("No encoded solution")?;

    // Handle simulation-only mode without private key
    if signer.is_none() {
        // Simulation without signing - use TransferFrom (approval simulated as tx)
        let tx = encode_tycho_router_call_no_permit(
            encoded_solution.clone(),
            &execution_solution,
            chain.native_token().address,
        )?;

        if cli.use_tenderly {
            run_tenderly_simulation(
                &tx,
                &sell_token_address,
                &amount_in,
                simulation_address,
                chain.id(),
                &order_solution.amount_out,
                &buy_token,
            )
            .await?;
        } else {
            // Create provider without wallet for simulation
            let provider = ProviderBuilder::default()
                .connect(&rpc_url)
                .await?;
            run_eth_simulation_no_wallet(
                &provider,
                &tx,
                &sell_token_address,
                &amount_in,
                simulation_address,
                chain.id(),
                &order_solution.amount_out,
                &buy_token,
            )
            .await?;
        }
        return Ok(());
    }

    // Full flow with private key (Permit2)
    let signer = signer.unwrap();

    // Encode the router call with permit signing
    let tx = encode_tycho_router_call(
        chain.id(),
        encoded_solution.clone(),
        &execution_solution,
        chain.native_token().address,
        signer.clone(),
    )?;

    // Create provider with wallet
    let tx_signer = EthereumWallet::from(signer.clone());
    let provider = ProviderBuilder::default()
        .with_chain(NamedChain::try_from(chain.id())?)
        .wallet(tx_signer)
        .connect(&rpc_url)
        .await?;

    // Show options
    if cli.simulate_only {
        // Run simulation directly
        run_simulation(
            &provider,
            &tx,
            &sell_token_address,
            &amount_in,
            signer.address(),
            chain.id(),
            cli.use_tenderly,
            &order_solution.amount_out,
            &buy_token,
        )
        .await?;
    } else {
        // Interactive prompt
        println!("\nWhat would you like to do?");
        let options = vec!["Simulate the swap", "Execute the swap", "Cancel"];
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose an action")
            .default(0)
            .items(&options)
            .interact()?;

        match selection {
            0 => {
                run_simulation(
                    &provider,
                    &tx,
                    &sell_token_address,
                    &amount_in,
                    signer.address(),
                    chain.id(),
                    cli.use_tenderly,
                    &order_solution.amount_out,
                    &buy_token,
                )
                .await?;
            }
            1 => {
                // Execute
                execute_swap(
                    &provider,
                    &tx,
                    &sell_token_address,
                    &amount_in,
                    signer.address(),
                    chain.id(),
                )
                .await?;
            }
            _ => {
                println!("Cancelled.");
            }
        }
    }

    Ok(())
}

async fn check_solver_health(
    client: &reqwest::Client,
    solver_url: &str,
) -> Result<HealthStatus, Box<dyn std::error::Error>> {
    let url = format!("{}/v1/health", solver_url);
    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        return Err(format!("Health check failed: {}", resp.status()).into());
    }

    Ok(resp.json().await?)
}

/// Fetch all available protocol systems from the Tycho API.
async fn fetch_protocol_systems(
    tycho_rpc: &HttpRPCClient,
    chain: Chain,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use tycho_simulation::tycho_common::dto::ProtocolSystemsRequestBody;

    const PAGE_SIZE: i64 = 100;
    let mut all_protocols = Vec::new();
    let mut page = 0;

    loop {
        let request = ProtocolSystemsRequestBody {
            chain: chain.into(),
            pagination: PaginationParams { page, page_size: PAGE_SIZE },
        };

        let response = tycho_rpc
            .get_protocol_systems(&request)
            .await
            .map_err(|e| format!("Failed to fetch protocol systems: {}", e))?;

        let count = response.protocol_systems.len();
        all_protocols.extend(response.protocol_systems);

        if (count as i64) < PAGE_SIZE {
            break;
        }
        page += 1;
    }

    // Filter out RFQ protocols (they start with "rfq:")
    let amm_protocols: Vec<String> = all_protocols
        .into_iter()
        .filter(|p| !p.starts_with("rfq:"))
        .collect();

    info!("Found {} AMM protocol systems", amm_protocols.len());
    Ok(amm_protocols)
}

/// Fetch AMM protocol components via tycho-client (paginated).
async fn fetch_amm_components(
    tycho_rpc: &HttpRPCClient,
    chain: Chain,
    protocol_systems: &[String],
    tvl_threshold: f64,
) -> Result<HashMap<String, ProtocolComponent>, Box<dyn std::error::Error>> {
    let mut all_components = HashMap::new();

    // Filter out RFQ protocols (they start with "rfq:")
    // TODO: Remove balancerv3 filtering
    let amm_protocols: Vec<_> = protocol_systems
        .iter()
        .filter(|p| {
            !p.starts_with("rfq:") && !p.contains("balancer_v3") && !p.contains("rocketpool")
        })
        .collect();

    for protocol in amm_protocols {
        let request = ProtocolComponentsRequestBody {
            protocol_system: protocol.clone(),
            component_ids: None,
            tvl_gt: Some(tvl_threshold),
            chain: chain.into(),
            pagination: PaginationParams { page: 0, page_size: 1000 },
        };

        let response = tycho_rpc
            .get_protocol_components_paginated(&request, Some(1000), 4)
            .await
            .map_err(|e| format!("Failed to fetch components for {}: {}", protocol, e))?;

        for dto in response.protocol_components {
            let pc = ProtocolComponent {
                id: dto.id.clone(),
                protocol_system: dto.protocol_system,
                protocol_type_name: dto.protocol_type_name,
                chain,
                tokens: dto.tokens,
                contract_addresses: dto.contract_ids,
                static_attributes: dto.static_attributes,
                change: Default::default(),
                creation_tx: dto.creation_tx,
                created_at: dto.created_at,
            };
            all_components.insert(pc.id.clone(), pc);
        }
    }

    Ok(all_components)
}

async fn get_solver_quote(
    client: &reqwest::Client,
    solver_url: &str,
    token_in: &Bytes,
    token_out: &Bytes,
    amount: &BigUint,
    sender: &str,
) -> Result<Solution, Box<dyn std::error::Error>> {
    let url = format!("{}/v1/solve", solver_url);

    let request = SolutionRequest {
        orders: vec![Order {
            id: String::new(), // Will be auto-generated by the server
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount: amount.clone(),
            side: OrderSide::Sell,
            sender: Bytes::from_str(sender)?,
            receiver: None,
        }],
        options: SolutionOptions { timeout_ms: Some(5000), ..Default::default() },
    };

    let resp = client
        .post(&url)
        .json(&request)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Solve request failed ({}): {}", status, body).into());
    }

    Ok(resp.json().await?)
}

fn display_quote(
    quote: &Solution,
    sell_token: &Token,
    buy_token: &Token,
    amount_in: &BigUint,
    all_tokens: &HashMap<Bytes, Token>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n========== Quote ==========");

    for order in &quote.orders {
        println!("Status: {:?}", order.status);

        if !matches!(order.status, fynd_core::SolutionStatus::Success) {
            println!("No route found for this order.");
            continue;
        }

        let formatted_in = format_token_amount(amount_in, sell_token);
        let formatted_out = format_token_amount(&order.amount_out, buy_token);

        println!(
            "Swap: {} {} -> {} {}",
            formatted_in, sell_token.symbol, formatted_out, buy_token.symbol
        );

        // Price
        let price = calculate_price(amount_in, &order.amount_out, sell_token, buy_token);
        println!("Price: {:.6} {} per {}", price, buy_token.symbol, sell_token.symbol);

        // Gas estimate
        println!("Gas estimate: {}", order.gas_estimate);

        // Price impact
        if let Some(impact) = order.price_impact_bps {
            let impact_percent = impact as f64 / 100.0;
            println!("Price impact: {:.2}%", impact_percent);
        }

        // Route details with token symbols
        if let Some(route) = &order.route {
            println!("\nRoute ({} hops):", route.swaps.len());
            for (i, swap) in route.swaps.iter().enumerate() {
                let token_in = all_tokens
                    .get(&swap.token_in)
                    .ok_or_else(|| format!("Token not found: {}", swap.token_in))?;
                let token_out = all_tokens
                    .get(&swap.token_out)
                    .ok_or_else(|| format!("Token not found: {}", swap.token_out))?;
                println!(
                    "  {}. {} -> {} via {} (pool: {})",
                    i + 1,
                    token_in.symbol,
                    token_out.symbol,
                    swap.protocol,
                    swap.component_id.clone()
                );
            }
        }
    }

    println!("\nSolve time: {}ms", quote.solve_time_ms);
    println!("Total gas: {}", quote.total_gas_estimate);
    println!("============================\n");

    Ok(())
}

fn format_token_amount(amount: &BigUint, token: &Token) -> String {
    let decimal_amount = amount.to_f64().unwrap_or(0.0) / 10f64.powi(token.decimals as i32);
    format!("{:.6}", decimal_amount)
}

fn format_u256_amount(amount: U256, token: &Token) -> String {
    let amount_f64 = amount
        .to_be_bytes::<32>()
        .iter()
        .fold(0f64, |acc, &b| acc * 256.0 + b as f64);
    let decimal_amount = amount_f64 / 10f64.powi(token.decimals as i32);
    format!("{:.6}", decimal_amount)
}

fn display_simulation_output(return_data: &[u8], expected_amount_out: &BigUint, buy_token: &Token) {
    // TychoRouter swap functions return uint256 amountOut (32 bytes ABI-encoded)
    let actual_amount = U256::from_be_slice(&return_data[..32]);
    let expected = biguint_to_u256(expected_amount_out);

    println!(
        "\n  Simulation output: {} {}",
        format_u256_amount(actual_amount, buy_token),
        buy_token.symbol
    );
    println!(
        "  Solver expected:   {} {}",
        format_token_amount(expected_amount_out, buy_token),
        buy_token.symbol
    );

    if actual_amount != expected {
        // Calculate difference in basis points
        let diff_bps = calculate_diff_bps(actual_amount, expected);
        println!("  Difference: {:.2}%", diff_bps / 100.0);
    } else {
        println!("  Difference: 0% (exact match)");
    }
}

fn calculate_diff_bps(actual: U256, expected: U256) -> f64 {
    if expected.is_zero() {
        return 0.0;
    }

    let actual_f64 = actual
        .to_be_bytes::<32>()
        .iter()
        .fold(0f64, |acc, &b| acc * 256.0 + b as f64);
    let expected_f64 = expected
        .to_be_bytes::<32>()
        .iter()
        .fold(0f64, |acc, &b| acc * 256.0 + b as f64);

    let diff = if actual_f64 > expected_f64 {
        actual_f64 - expected_f64
    } else {
        expected_f64 - actual_f64
    };

    (diff / expected_f64) * 10_000.0 // basis points
}

fn calculate_price(
    amount_in: &BigUint,
    amount_out: &BigUint,
    token_in: &Token,
    token_out: &Token,
) -> f64 {
    let decimal_in = amount_in.to_f64().unwrap_or(0.0) / 10f64.powi(token_in.decimals as i32);
    let decimal_out = amount_out.to_f64().unwrap_or(0.0) / 10f64.powi(token_out.decimals as i32);

    if decimal_in > 0.0 {
        decimal_out / decimal_in
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
fn map_route_to_execution_solution(
    route: &Route,
    components: &HashMap<String, ProtocolComponent>,
    sell_token: &Token,
    buy_token: &Token,
    amount_in: &BigUint,
    user_address: Bytes,
    slippage_bps: u32,
) -> Result<ExecutionSolution, Box<dyn std::error::Error>> {
    let mut swaps = Vec::new();

    for solver_swap in &route.swaps {
        // Look up the component from our fetched data
        let component = components
            .get(&solver_swap.component_id)
            .ok_or_else(|| {
                format!(
                "Component not found: {}. This component may not have been fetched from the API. \
                Try adjusting --tvl-threshold or ensuring the protocol is included in --protocols.",
                solver_swap.component_id
            )
            })?;

        // Use the conversion trait
        let execution_swap = solver_swap.to_execution_swap(component);
        swaps.push(execution_swap);
    }

    // Calculate minimum amount out with slippage
    let last_swap = route
        .swaps
        .last()
        .ok_or("Empty route")?;
    let checked_amount = calculate_min_amount_out(&last_swap.amount_out, slippage_bps);

    Ok(ExecutionSolution {
        sender: user_address.clone(),
        receiver: user_address,
        given_token: sell_token.address.clone(),
        given_amount: amount_in.clone(),
        checked_token: buy_token.address.clone(),
        exact_out: false,
        checked_amount,
        swaps,
        ..Default::default()
    })
}

fn calculate_min_amount_out(expected_amount: &BigUint, slippage_bps: u32) -> BigUint {
    let bps = BigUint::from(10_000u32);
    let slippage = BigUint::from(slippage_bps);
    let multiplier = &bps - &slippage;
    (expected_amount * &multiplier) / &bps
}

fn encode_tycho_router_call(
    chain_id: u64,
    encoded_solution: EncodedSolution,
    solution: &ExecutionSolution,
    native_address: Bytes,
    signer: PrivateKeySigner,
) -> Result<Transaction, Box<dyn std::error::Error>> {
    let permit_data = encoded_solution
        .permit
        .as_ref()
        .ok_or("Permit object must be set")?;

    let permit = PermitSingle::try_from(permit_data)?;
    let signature = sign_permit(chain_id, permit_data, signer)?;

    let given_amount = biguint_to_u256(&solution.given_amount);
    let min_amount_out = biguint_to_u256(&solution.checked_amount);
    let given_token = Address::from_slice(&solution.given_token);
    let checked_token = Address::from_slice(&solution.checked_token);
    let receiver = Address::from_slice(&solution.receiver);

    let method_calldata = (
        given_amount,
        given_token,
        checked_token,
        min_amount_out,
        false,
        false,
        receiver,
        permit,
        signature.as_bytes().to_vec(),
        encoded_solution.swaps.clone(),
    )
        .abi_encode();

    let contract_interaction = encode_input(&encoded_solution.function_signature, method_calldata);

    let value = if solution.given_token == native_address {
        solution.given_amount.clone()
    } else {
        BigUint::ZERO
    };

    Ok(Transaction { to: encoded_solution.interacting_with, value, data: contract_interaction })
}

/// Encode router call without permit signing (for TransferFrom mode simulation).
fn encode_tycho_router_call_no_permit(
    encoded_solution: EncodedSolution,
    solution: &ExecutionSolution,
    native_address: Bytes,
) -> Result<Transaction, Box<dyn std::error::Error>> {
    println!("\nEncoding for TransferFrom mode:");
    println!("  Function signature: {}", encoded_solution.function_signature);
    println!("  Interacting with: 0x{}", hex::encode(&encoded_solution.interacting_with));
    println!("  Swaps bytes length: {} bytes", encoded_solution.swaps.len());

    let given_amount = biguint_to_u256(&solution.given_amount);
    let min_amount_out = biguint_to_u256(&solution.checked_amount);
    let given_token = Address::from_slice(&solution.given_token);
    let checked_token = Address::from_slice(&solution.checked_token);
    let receiver = Address::from_slice(&solution.receiver);

    println!("  Given amount: {}", given_amount);
    println!("  Min amount out: {}", min_amount_out);
    println!("  Given token: {:?}", given_token);
    println!("  Checked token: {:?}", checked_token);
    println!("  Receiver: {:?}", receiver);

    // For TransferFrom mode, encode with transfer_from boolean
    // Function signature: singleSwap(uint256,address,address,uint256,bool,bool,address,bool,bytes)
    // or sequentialSwap(...) or splitSwap(...) with similar pattern
    // Parameters: given_amount, given_token, checked_token, min_amount_out, wrap, unwrap, receiver,
    // transfer_from, swaps
    let method_calldata = (
        given_amount,
        given_token,
        checked_token,
        min_amount_out,
        false, // wrap (native input)
        false, // unwrap (native output)
        receiver,
        true, // transfer_from - IMPORTANT: must be true for TransferFrom mode
        encoded_solution.swaps.clone(),
    )
        .abi_encode();

    let contract_interaction = encode_input(&encoded_solution.function_signature, method_calldata);

    let value = if solution.given_token == native_address {
        solution.given_amount.clone()
    } else {
        BigUint::ZERO
    };

    Ok(Transaction { to: encoded_solution.interacting_with, value, data: contract_interaction })
}

fn sign_permit(
    chain_id: u64,
    permit_single: &tycho_execution::encoding::models::PermitSingle,
    signer: PrivateKeySigner,
) -> Result<Signature, Box<dyn std::error::Error>> {
    // Permit2 is deployed at the same address on all supported chains
    let permit2_address = Address::from_str("0x000000000022D473030F116dDEE9F6B43aC78BA3")?;
    let domain = eip712_domain! {
        name: "Permit2",
        chain_id: chain_id,
        verifying_contract: permit2_address,
    };

    let permit_single = PermitSingle::try_from(permit_single)?;
    let hash = permit_single.eip712_signing_hash(&domain);

    signer
        .sign_hash_sync(&hash)
        .map_err(|e| format!("Failed to sign permit: {}", e).into())
}

fn encode_input(selector: &str, mut encoded_args: Vec<u8>) -> Vec<u8> {
    let mut hasher = Keccak256::new();
    hasher.update(selector.as_bytes());
    let selector_bytes = &hasher.finalize()[..4];
    let mut call_data = selector_bytes.to_vec();

    // Remove extra prefix if present
    if encoded_args.len() > 32 &&
        encoded_args[..32] ==
            [0u8; 31]
                .into_iter()
                .chain([32].to_vec())
                .collect::<Vec<u8>>()
    {
        encoded_args = encoded_args[32..].to_vec();
    }

    call_data.extend(encoded_args);
    call_data
}

/// Run simulation with either eth_simulate or Tenderly.
#[allow(clippy::too_many_arguments)]
async fn run_simulation(
    provider: &FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    tx: &Transaction,
    sell_token_address: &Bytes,
    amount_in: &BigUint,
    user_address: Address,
    chain_id: u64,
    use_tenderly: bool,
    expected_amount_out: &BigUint,
    buy_token: &Token,
) -> Result<(), Box<dyn std::error::Error>> {
    if use_tenderly {
        run_tenderly_simulation(
            tx,
            sell_token_address,
            amount_in,
            user_address,
            chain_id,
            expected_amount_out,
            buy_token,
        )
        .await
    } else {
        run_eth_simulation(
            provider,
            tx,
            sell_token_address,
            amount_in,
            user_address,
            chain_id,
            expected_amount_out,
            buy_token,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_eth_simulation(
    provider: &FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    tx: &Transaction,
    sell_token_address: &Bytes,
    amount_in: &BigUint,
    user_address: Address,
    chain_id: u64,
    expected_amount_out: &BigUint,
    buy_token: &Token,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\nSimulating via eth_simulate...");

    let (approval_request, swap_request) = build_tx_requests(
        provider,
        biguint_to_u256(amount_in),
        user_address,
        Address::from_slice(sell_token_address),
        tx.clone(),
        chain_id,
    )
    .await?;

    let payload = SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: None,
            state_overrides: None,
            calls: vec![approval_request, swap_request],
        }],
        trace_transfers: true,
        validation: true,
        return_full_transactions: true,
    };

    match provider.simulate(&payload).await {
        Ok(output) => {
            println!("\nSimulation Results:");
            let mut all_success = true;
            for block in output.iter() {
                println!("Block {}:", block.inner.header.number);
                for (j, transaction) in block.calls.iter().enumerate() {
                    let tx_name = if j == 0 { "Approval" } else { "Swap" };
                    let status_ok = transaction.status;
                    if !status_ok {
                        all_success = false;
                    }
                    println!(
                        "  {}: Status={}, Gas Used={}",
                        tx_name,
                        if status_ok { "Success" } else { "FAILED" },
                        transaction.gas_used
                    );

                    // Decode and display return data for successful swap
                    if j == 1 && status_ok && transaction.return_data.len() >= 32 {
                        display_simulation_output(
                            &transaction.return_data,
                            expected_amount_out,
                            buy_token,
                        );
                    }
                }
            }
            if all_success {
                println!("\nSimulation successful! The swap would execute correctly.");
            } else {
                println!("\nSimulation completed but some transactions failed.");
                println!("This likely means the sender lacks token balance or approvals.");
            }
        }
        Err(e) => {
            println!("\nSimulation failed: {:?}", e);
            println!("Your RPC provider may not support eth_simulate.");
            return Err(e.into());
        }
    }

    Ok(())
}

/// Run eth_simulate without a wallet provider (for --sender without private key).
#[allow(clippy::too_many_arguments)]
async fn run_eth_simulation_no_wallet(
    provider: &RootProvider<Ethereum>,
    tx: &Transaction,
    sell_token_address: &Bytes,
    amount_in: &BigUint,
    user_address: Address,
    chain_id: u64,
    expected_amount_out: &BigUint,
    buy_token: &Token,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\nSimulating via eth_simulate (no wallet)...");
    println!("  Router address: 0x{}", hex::encode(&tx.to));
    println!("  Calldata length: {} bytes", tx.data.len());
    println!(
        "  Calldata (first 100 bytes): 0x{}",
        hex::encode(&tx.data[..std::cmp::min(100, tx.data.len())])
    );

    // For TransferFrom mode, approve the router (tx.to) instead of Permit2
    let router_address = Address::from_slice(&tx.to);
    let (approval_request, swap_request) = build_tx_requests_simple(
        provider,
        biguint_to_u256(amount_in),
        user_address,
        Address::from_slice(sell_token_address),
        router_address,
        tx.clone(),
        chain_id,
    )
    .await?;

    let payload = SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: None,
            state_overrides: None,
            calls: vec![approval_request, swap_request],
        }],
        trace_transfers: true,
        validation: true,
        return_full_transactions: true,
    };

    match provider.simulate(&payload).await {
        Ok(output) => {
            println!("\nSimulation Results:");
            let mut all_success = true;
            for block in output.iter() {
                println!("Block {}:", block.inner.header.number);
                for (j, transaction) in block.calls.iter().enumerate() {
                    let tx_name = if j == 0 { "Approval" } else { "Swap" };
                    let status_ok = transaction.status;
                    if !status_ok {
                        all_success = false;
                    }
                    println!(
                        "  {}: Status={}, Gas Used={}",
                        tx_name,
                        if status_ok { "Success" } else { "FAILED" },
                        transaction.gas_used
                    );
                    // Print return data for debugging failed transactions
                    if !status_ok {
                        println!("    Return data: 0x{}", hex::encode(&transaction.return_data));
                    }

                    // Decode and display return data for successful swap
                    if j == 1 && status_ok && transaction.return_data.len() >= 32 {
                        display_simulation_output(
                            &transaction.return_data,
                            expected_amount_out,
                            buy_token,
                        );
                    }
                }
            }
            if all_success {
                println!("\nSimulation successful! The swap would execute correctly.");
            } else {
                println!("\nSimulation completed but some transactions failed.");
                println!("Check the return data above for error details.");
            }
        }
        Err(e) => {
            println!("\nSimulation failed: {:?}", e);
            println!("Your RPC provider may not support eth_simulate.");
            return Err(e.into());
        }
    }

    Ok(())
}

async fn run_tenderly_simulation(
    tx: &Transaction,
    sell_token_address: &Bytes,
    amount_in: &BigUint,
    user_address: Address,
    chain_id: u64,
    expected_amount_out: &BigUint,
    buy_token: &Token,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\nSimulating via Tenderly...");

    let access_key = env::var("TENDERLY_ACCESS_KEY").map_err(|_| "TENDERLY_ACCESS_KEY not set")?;
    let account = env::var("TENDERLY_ACCOUNT").map_err(|_| "TENDERLY_ACCOUNT not set")?;
    let project = env::var("TENDERLY_PROJECT").map_err(|_| "TENDERLY_PROJECT not set")?;

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.tenderly.co/api/v1/account/{}/project/{}/simulate-bundle",
        account, project
    );

    // Build approval transaction
    let approve_data = build_approval_calldata(amount_in);
    let approval_sim = TenderlySimulation {
        network_id: chain_id.to_string(),
        from: format!("{:?}", user_address),
        to: format!("0x{}", hex::encode(sell_token_address)),
        input: format!("0x{}", hex::encode(&approve_data)),
        value: "0".to_string(),
        save: false,
        save_if_fails: true,
    };

    // Build swap transaction
    let swap_sim = TenderlySimulation {
        network_id: chain_id.to_string(),
        from: format!("{:?}", user_address),
        to: format!("0x{}", hex::encode(&tx.to)),
        input: format!("0x{}", hex::encode(&tx.data)),
        value: tx.value.to_string(),
        save: false,
        save_if_fails: true,
    };

    let request = TenderlySimulationRequest { simulations: vec![approval_sim, swap_sim] };

    let resp = client
        .post(&url)
        .header("X-Access-Key", &access_key)
        .json(&request)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Tenderly simulation failed ({}): {}", status, body).into());
    }

    let result: TenderlySimulationResponse = resp.json().await?;

    println!("\nTenderly Simulation Results:");
    for (i, sim_result) in result
        .simulation_results
        .iter()
        .enumerate()
    {
        let tx_name = if i == 0 { "Approval" } else { "Swap" };
        let status = if sim_result.simulation.status { "Success" } else { "Failed" };
        println!("  {}: Status={}, Gas Used={}", tx_name, status, sim_result.simulation.gas_used);
        if let Some(ref err) = sim_result.simulation.error_message {
            println!("    Error: {}", err);
        }

        // Decode and display return data for successful swap
        if i == 1 && sim_result.simulation.status {
            if let Some(ref return_value) = sim_result.transaction.output {
                // Tenderly returns hex string, decode it
                let return_data =
                    hex::decode(return_value.trim_start_matches("0x")).unwrap_or_default();
                if return_data.len() >= 32 {
                    display_simulation_output(&return_data, expected_amount_out, buy_token);
                }
            }
        }
    }

    Ok(())
}

fn build_approval_calldata(amount: &BigUint) -> Vec<u8> {
    // Permit2 is deployed at the same address on all supported chains
    let permit2_address = Address::from_str("0x000000000022D473030F116dDEE9F6B43aC78BA3").unwrap();
    build_approval_calldata_for(amount, permit2_address)
}

fn build_approval_calldata_for(amount: &BigUint, spender: Address) -> Vec<u8> {
    let amount_u256 = biguint_to_u256(amount);
    let args = (spender, amount_u256).abi_encode();
    encode_input("approve(address,uint256)", args)
}

async fn build_tx_requests(
    provider: &FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    amount_in: U256,
    user_address: Address,
    sell_token_address: Address,
    tx: Transaction,
    chain_id: u64,
) -> Result<(TransactionRequest, TransactionRequest), Box<dyn std::error::Error>> {
    let block = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
        .await?
        .ok_or("Block not found")?;

    let base_fee = block
        .header
        .base_fee_per_gas
        .ok_or("No base fee")?;
    let max_priority_fee_per_gas = 1_000_000_000u64;
    let max_fee_per_gas = base_fee + max_priority_fee_per_gas;

    let nonce = provider
        .get_transaction_count(user_address)
        .await?;

    let approve_data =
        build_approval_calldata(&BigUint::from_bytes_be(&amount_in.to_be_bytes::<32>()));

    let approval_request = TransactionRequest {
        to: Some(TxKind::Call(sell_token_address)),
        from: Some(user_address),
        value: None,
        input: TransactionInput { input: Some(AlloyBytes::from(approve_data)), data: None },
        gas: Some(100_000u64),
        chain_id: Some(chain_id),
        max_fee_per_gas: Some(max_fee_per_gas.into()),
        max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
        nonce: Some(nonce),
        ..Default::default()
    };

    let swap_request = TransactionRequest {
        to: Some(TxKind::Call(Address::from_slice(&tx.to))),
        from: Some(user_address),
        value: Some(biguint_to_u256(&tx.value)),
        input: TransactionInput { input: Some(AlloyBytes::from(tx.data)), data: None },
        gas: Some(800_000u64),
        chain_id: Some(chain_id),
        max_fee_per_gas: Some(max_fee_per_gas.into()),
        max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
        nonce: Some(nonce + 1),
        ..Default::default()
    };

    Ok((approval_request, swap_request))
}

/// Build transaction requests for simulation without wallet (TransferFrom mode).
async fn build_tx_requests_simple(
    provider: &RootProvider<Ethereum>,
    amount_in: U256,
    user_address: Address,
    sell_token_address: Address,
    router_address: Address,
    tx: Transaction,
    chain_id: u64,
) -> Result<(TransactionRequest, TransactionRequest), Box<dyn std::error::Error>> {
    let block = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
        .await?
        .ok_or("Block not found")?;

    let base_fee = block
        .header
        .base_fee_per_gas
        .ok_or("No base fee")?;
    let max_priority_fee_per_gas = 1_000_000_000u64;
    let max_fee_per_gas = base_fee + max_priority_fee_per_gas;

    let nonce = provider
        .get_transaction_count(user_address)
        .await?;

    // For TransferFrom mode, approve the router (not Permit2)
    let approve_data = build_approval_calldata_for(
        &BigUint::from_bytes_be(&amount_in.to_be_bytes::<32>()),
        router_address,
    );

    let approval_request = TransactionRequest {
        to: Some(TxKind::Call(sell_token_address)),
        from: Some(user_address),
        value: None,
        input: TransactionInput { input: Some(AlloyBytes::from(approve_data)), data: None },
        gas: Some(100_000u64),
        chain_id: Some(chain_id),
        max_fee_per_gas: Some(max_fee_per_gas.into()),
        max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
        nonce: Some(nonce),
        ..Default::default()
    };

    let swap_request = TransactionRequest {
        to: Some(TxKind::Call(Address::from_slice(&tx.to))),
        from: Some(user_address),
        value: Some(biguint_to_u256(&tx.value)),
        input: TransactionInput { input: Some(AlloyBytes::from(tx.data)), data: None },
        gas: Some(800_000u64),
        chain_id: Some(chain_id),
        max_fee_per_gas: Some(max_fee_per_gas.into()),
        max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
        nonce: Some(nonce + 1),
        ..Default::default()
    };

    Ok((approval_request, swap_request))
}

async fn execute_swap(
    provider: &FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    tx: &Transaction,
    sell_token_address: &Bytes,
    amount_in: &BigUint,
    user_address: Address,
    chain_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\nExecuting swap...");

    let (approval_request, swap_request) = build_tx_requests(
        provider,
        biguint_to_u256(amount_in),
        user_address,
        Address::from_slice(sell_token_address),
        tx.clone(),
        chain_id,
    )
    .await?;

    // Send approval
    println!("Sending approval transaction...");
    let approval_receipt = provider
        .send_transaction(approval_request)
        .await?;
    let approval_result = approval_receipt.get_receipt().await?;
    println!(
        "Approval tx: {:?}, status: {:?}",
        approval_result.transaction_hash,
        approval_result.status()
    );

    if !approval_result.status() {
        return Err("Approval transaction failed".into());
    }

    // Send swap
    println!("Sending swap transaction...");
    let swap_receipt = provider
        .send_transaction(swap_request)
        .await?;
    let swap_result = swap_receipt.get_receipt().await?;
    println!("Swap tx: {:?}, status: {:?}", swap_result.transaction_hash, swap_result.status());

    if !swap_result.status() {
        return Err("Swap transaction failed".into());
    }

    println!("\nSwap executed successfully!");
    Ok(())
}
