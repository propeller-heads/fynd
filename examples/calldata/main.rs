//! Calldata Generator: Get transaction calldata from Fynd for browser wallet signing.
//!
//! This example:
//! 1. Gets a swap quote from a running Fynd solver
//! 2. Encodes the transaction via tycho-execution (TransferFrom mode)
//! 3. Prints the raw calldata you can paste into MetaMask or any browser wallet
//!
//! Usage:
//!   # Start the solver first, then:
//!   cargo run --example calldata -- --sender 0xYourWallet --sell-amount 100

use std::{collections::HashMap, env, str::FromStr};

use alloy::{
    primitives::{Address, Keccak256},
    sol_types::SolValue,
};
use clap::Parser;
use fynd::{
    parse_chain, types::solution::OrderSide, HealthStatus, Order, Route, Solution, SolutionOptions,
    SolutionRequest,
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::{Solution as ExecutionSolution, Transaction, UserTransferType},
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

// Re-use the SwapToExecution trait from the tutorial
mod types {
    use tycho_simulation::tycho_common::{models::protocol::ProtocolComponent, Bytes};

    pub trait SwapToExecution {
        fn to_execution_swap(
            &self,
            component: &ProtocolComponent,
        ) -> tycho_execution::encoding::models::Swap;
    }

    impl SwapToExecution for fynd::Swap {
        fn to_execution_swap(
            &self,
            component: &ProtocolComponent,
        ) -> tycho_execution::encoding::models::Swap {
            tycho_execution::encoding::models::Swap::new(
                component.clone(),
                Bytes::from(self.token_in.as_ref()),
                Bytes::from(self.token_out.as_ref()),
            )
        }
    }
}

use types::SwapToExecution;

/// Generate calldata for browser wallet signing
#[derive(Parser)]
#[command(name = "calldata")]
#[command(about = "Get swap calldata from Fynd for browser wallet signing")]
struct Cli {
    /// Your wallet address (the sender/signer)
    #[arg(long)]
    sender: String,

    /// Sell token address (defaults to USDC)
    #[arg(long, default_value = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")]
    sell_token: String,

    /// Buy token address (defaults to WETH)
    #[arg(long, default_value = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")]
    buy_token: String,

    /// Amount to sell in human-readable units (e.g., 100 for 100 USDC)
    #[arg(long, default_value_t = 100.0)]
    sell_amount: f64,

    /// Blockchain network
    #[arg(long, default_value = "ethereum")]
    chain: String,

    /// Solver API URL
    #[arg(long, default_value = "http://localhost:3000")]
    solver_url: String,

    /// Min pool TVL in ETH
    #[arg(long, default_value_t = 10.0)]
    tvl_threshold: f64,

    /// Slippage tolerance in basis points (50 = 0.5%)
    #[arg(long, default_value_t = 50)]
    slippage_bps: u32,

    /// Protocol systems (comma-separated). If not set, fetches all from API.
    #[arg(long)]
    protocols: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();

    let cli = Cli::parse();
    let chain = parse_chain(&cli.chain)?;

    let tycho_url =
        env::var("TYCHO_URL").unwrap_or_else(|_| "tycho-beta.propellerheads.xyz".to_string());
    let tycho_api_key = env::var("TYCHO_API_KEY").ok();

    // Create Tycho RPC client
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(tycho_api_key.clone());
    let tycho_rpc = HttpRPCClient::new(&format!("https://{}", tycho_url), rpc_options)
        .map_err(|e| format!("Failed to create Tycho RPC client: {}", e))?;

    // Determine protocols
    let protocol_systems = match &cli.protocols {
        Some(protocols) => protocols
            .split(',')
            .map(|s| s.trim().to_string())
            .collect(),
        None => {
            eprintln!("Fetching available protocols...");
            fetch_protocol_systems(&tycho_rpc, chain).await?
        }
    };

    // Check solver health
    let client = reqwest::Client::new();
    let health: HealthStatus = client
        .get(format!("{}/v1/health", cli.solver_url))
        .send()
        .await?
        .json()
        .await?;

    if !health.healthy {
        return Err("Solver is not healthy. Wait for market data to load.".into());
    }

    // Load tokens
    eprintln!("Loading tokens...");
    let all_tokens =
        load_all_tokens(&tycho_url, false, tycho_api_key.as_deref(), true, chain, None, None)
            .await?;

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

    let amount_in =
        BigUint::from((cli.sell_amount * 10f64.powi(sell_token.decimals as i32)) as u128);

    // Get quote from solver
    eprintln!(
        "Getting quote: {} {} -> {}...",
        cli.sell_amount, sell_token.symbol, buy_token.symbol
    );

    let request = SolutionRequest {
        orders: vec![Order {
            id: String::new(),
            token_in: sell_token_address.clone(),
            token_out: buy_token_address.clone(),
            amount: amount_in.clone(),
            side: OrderSide::Sell,
            sender: Bytes::from_str(&cli.sender)?,
            receiver: None,
        }],
        options: SolutionOptions { timeout_ms: Some(5000), ..Default::default() },
    };

    let quote: Solution = client
        .post(format!("{}/v1/solve", cli.solver_url))
        .json(&request)
        .send()
        .await?
        .json()
        .await?;

    let order = quote
        .orders
        .first()
        .ok_or("No order in response")?;
    if !matches!(order.status, fynd::SolutionStatus::Success) {
        return Err(format!("No route found (status: {:?})", order.status).into());
    }

    let route = order
        .route
        .as_ref()
        .ok_or("No route in solution")?;

    // Display the quote
    let formatted_in = format_amount(&amount_in, &sell_token);
    let formatted_out = format_amount(&order.amount_out, &buy_token);
    eprintln!();
    eprintln!("========== QUOTE ==========");
    eprintln!("  {} {} -> {} {}", formatted_in, sell_token.symbol, formatted_out, buy_token.symbol);
    eprintln!("  Route: {} hop(s)", route.swaps.len());
    for (i, swap) in route.swaps.iter().enumerate() {
        let tin = all_tokens
            .get(&swap.token_in)
            .map(|t| t.symbol.as_str())
            .unwrap_or("?");
        let tout = all_tokens
            .get(&swap.token_out)
            .map(|t| t.symbol.as_str())
            .unwrap_or("?");
        eprintln!(
            "    {}. {} -> {} via {} ({})",
            i + 1,
            tin,
            tout,
            swap.protocol,
            swap.component_id
        );
    }
    eprintln!("  Gas estimate: {}", order.gas_estimate);
    eprintln!("  Solve time: {}ms", quote.solve_time_ms);
    eprintln!("===========================");

    // Fetch protocol components for encoding
    eprintln!("\nEncoding transaction...");
    let components =
        fetch_amm_components(&tycho_rpc, chain, &protocol_systems, cli.tvl_threshold).await?;

    // Map route to execution solution
    let sender_bytes = Bytes::from_str(&cli.sender)?;
    let execution_solution = map_route_to_execution(
        route,
        &components,
        &sell_token,
        &buy_token,
        &amount_in,
        sender_bytes,
        cli.slippage_bps,
    )?;

    // Encode
    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .expect("Failed to get default SwapEncoderRegistry");

    let encoder = TychoRouterEncoderBuilder::new()
        .chain(chain)
        .user_transfer_type(UserTransferType::TransferFrom)
        .swap_encoder_registry(swap_encoder_registry)
        .build()?;

    let encoded = encoder.encode_solutions(vec![execution_solution.clone()])?;
    let encoded_solution = encoded
        .into_iter()
        .next()
        .ok_or("No encoded solution")?;

    // Build the raw transaction
    let tx = encode_transfer_from_tx(
        encoded_solution,
        &execution_solution,
        chain.native_token().address,
    )?;

    let router_address = format!("0x{}", hex::encode(&tx.to));

    // Build the approval calldata
    let approve_calldata = build_approve_calldata(&amount_in, Address::from_str(&router_address)?);

    // Print everything in a MetaMask-friendly format
    println!();
    println!("============================================================");
    println!("  TRANSACTION CALLDATA FOR BROWSER WALLET");
    println!("============================================================");
    println!();
    println!("STEP 1: APPROVE (skip if you already approved this router)");
    println!("  To:       {}", cli.sell_token);
    println!("  Value:    0");
    println!("  Data:     0x{}", hex::encode(&approve_calldata));
    println!();
    println!("STEP 2: SWAP");
    println!("  To:       {}", router_address);
    println!("  Value:    {}", tx.value);
    println!("  Data:     0x{}", hex::encode(&tx.data));
    println!();
    println!("============================================================");
    println!("  DETAILS");
    println!("============================================================");
    println!("  Sell:           {} {}", formatted_in, sell_token.symbol);
    println!("  Buy (expected): {} {}", formatted_out, buy_token.symbol);
    println!(
        "  Min out ({}bps slip): {} {}",
        cli.slippage_bps,
        format_amount(&execution_solution.checked_amount, &buy_token),
        buy_token.symbol
    );
    println!("  Router:         {}", router_address);
    println!("  Block:          {}", order.block.number);
    println!("============================================================");

    Ok(())
}

fn format_amount(amount: &BigUint, token: &Token) -> String {
    let decimal_amount = amount.to_f64().unwrap_or(0.0) / 10f64.powi(token.decimals as i32);
    format!("{:.6}", decimal_amount)
}

fn encode_input(selector: &str, mut encoded_args: Vec<u8>) -> Vec<u8> {
    let mut hasher = Keccak256::new();
    hasher.update(selector.as_bytes());
    let selector_bytes = &hasher.finalize()[..4];
    let mut call_data = selector_bytes.to_vec();

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

fn build_approve_calldata(amount: &BigUint, spender: Address) -> Vec<u8> {
    let amount_u256 = biguint_to_u256(amount);
    let args = (spender, amount_u256).abi_encode();
    encode_input("approve(address,uint256)", args)
}

fn encode_transfer_from_tx(
    encoded_solution: tycho_execution::encoding::models::EncodedSolution,
    solution: &ExecutionSolution,
    native_address: Bytes,
) -> Result<Transaction, Box<dyn std::error::Error>> {
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
        false, // wrap
        false, // unwrap
        receiver,
        true, // transfer_from = true
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

fn map_route_to_execution(
    route: &Route,
    components: &HashMap<String, ProtocolComponent>,
    sell_token: &Token,
    buy_token: &Token,
    amount_in: &BigUint,
    sender: Bytes,
    slippage_bps: u32,
) -> Result<ExecutionSolution, Box<dyn std::error::Error>> {
    let mut swaps = Vec::new();
    for solver_swap in &route.swaps {
        let component = components
            .get(&solver_swap.component_id)
            .ok_or_else(|| {
                format!(
                    "Component not found: {}. Try adjusting --tvl-threshold or --protocols.",
                    solver_swap.component_id
                )
            })?;
        swaps.push(solver_swap.to_execution_swap(component));
    }

    let last_swap = route
        .swaps
        .last()
        .ok_or("Empty route")?;
    let bps = BigUint::from(10_000u32);
    let slippage = BigUint::from(slippage_bps);
    let checked_amount = (&last_swap.amount_out * (&bps - &slippage)) / &bps;

    Ok(ExecutionSolution {
        sender: sender.clone(),
        receiver: sender,
        given_token: sell_token.address.clone(),
        given_amount: amount_in.clone(),
        checked_token: buy_token.address.clone(),
        exact_out: false,
        checked_amount,
        swaps,
        ..Default::default()
    })
}

async fn fetch_protocol_systems(
    tycho_rpc: &HttpRPCClient,
    chain: Chain,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use tycho_simulation::tycho_common::dto::ProtocolSystemsRequestBody;

    let mut all_protocols = Vec::new();
    let mut page = 0;
    loop {
        let request = ProtocolSystemsRequestBody {
            chain: chain.into(),
            pagination: PaginationParams { page, page_size: 100 },
        };
        let response = tycho_rpc
            .get_protocol_systems(&request)
            .await
            .map_err(|e| format!("Failed to fetch protocol systems: {}", e))?;
        let count = response.protocol_systems.len();
        all_protocols.extend(response.protocol_systems);
        if (count as i64) < 100 {
            break;
        }
        page += 1;
    }
    Ok(all_protocols
        .into_iter()
        .filter(|p| !p.starts_with("rfq:"))
        .collect())
}

async fn fetch_amm_components(
    tycho_rpc: &HttpRPCClient,
    chain: Chain,
    protocol_systems: &[String],
    tvl_threshold: f64,
) -> Result<HashMap<String, ProtocolComponent>, Box<dyn std::error::Error>> {
    let mut all_components = HashMap::new();
    let amm_protocols: Vec<_> = protocol_systems
        .iter()
        .filter(|p| !p.starts_with("rfq:") && !p.contains("balancer_v3"))
        .cloned()
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
