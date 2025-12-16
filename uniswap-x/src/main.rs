mod gateway;
mod models;
mod order;
mod orderbook;
mod solver;

use models::UniswapXConfig;
use solver::{UniswapXSolver, UniswapXSolverConfig};
use gateway::UniswapXGateway;
use tycho_router::{
    modules::{
        algorithm::most_liquid::MostLiquidAlgorithm,
        execution::executor::Executor,
    },
    solver::Solver,
};
use tycho_execution::encoding::models::UserTransferType;
use tycho_simulation::tycho_common::{models::Chain, Bytes};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Environment variables:
    // - POLL_INTERVAL_SECS: Order polling interval (default: 10s)
    // - TYCHO_URL: Tycho WebSocket URL (default: wss://api.tycho.org)
    // - TYCHO_API_KEY: Tycho API authentication key (default: demo_key)
    // - RPC_URL: Ethereum RPC endpoint (default: https://eth.llamarpc.com)
    // - MIN_TVL: Minimum TVL threshold for protocol filtering (default: 10000.0)
    // - MAX_TVL: Maximum TVL threshold for protocol filtering (default: 1000000000.0)
    
    println!("UniswapX Router Integration");
    println!("===============================");

    // Configuration
    let uniswap_config = UniswapXConfig {
        api_endpoint: "https://api.uniswap.org/v2/orders".to_string(),
        api_key: std::env::var("UNISWAP_X_API_KEY").ok(),
        chain_id: 1, // Ethereum mainnet
        timeout_secs: 30,
        max_orders_per_request: 100,
        filler_address: Bytes(hex::decode("6D9da78B6A5BEdcA287AA5d49613bA36b90c15C4").unwrap().into()),
        usx_reactor: Bytes(hex::decode("00000011F84B9aa48e5f8aA8B9897600006289Be").unwrap().into()),

    };

    let solver_config = UniswapXSolverConfig { 
        min_profit_threshold: 0.00001 // 0.01 ETH minimum profit
    };

    // Show configuration
    println!("Configuration:");
    println!("  Chain: Ethereum Mainnet ({})", uniswap_config.chain_id);
    println!("  Min Profit: {:.5} ETH", solver_config.min_profit_threshold);
    println!("  UniswapX API: {}", 
        if uniswap_config.api_key.is_some() { "Authenticated" } else { "No API key (rate limited)" });
    println!("  Tycho API: {}", 
        std::env::var("TYCHO_API_KEY").unwrap_or_else(|_| "demo_key".to_string()));
    println!("  RPC: {}", 
        std::env::var("RPC_URL").unwrap_or_else(|_| "https://eth.llamarpc.com".to_string()));
    println!();

    println!("Initializing components...");

    // Initialize tycho-router Solver with MostLiquidAlgorithm
    // This creates the routing engine that finds optimal paths through DEX liquidity
    let tycho_solver = Solver::<MostLiquidAlgorithm>::new(
        3, // max_hops - maximum number of DEX hops in a route
        Chain::Ethereum,
        std::env::var("TYCHO_URL")
            .unwrap_or_else(|_| "wss://api.tycho.org".to_string()),
        std::env::var("TYCHO_API_KEY")
            .unwrap_or_else(|_| "demo_key".to_string()),
        None,
        None, // tokens will be loaded from Tycho automatically
        (
            std::env::var("MIN_TVL")
                .unwrap_or_else(|_| "10000.0".to_string())
                .parse::<f64>()
                .unwrap_or(10000.0),
            std::env::var("MAX_TVL")
                .unwrap_or_else(|_| "1000000000.0".to_string())
                .parse::<f64>()
                .unwrap_or(1000000000.0),
        ), // tvl_threshold - (min_tvl, max_tvl) for protocol filtering
    ).await?;


    let router_address = Bytes(hex::decode("fD0b31d2E955fA55e3fa641Fe90e08b677188d35")
        .expect("Failed to decode router address").into());

    // Initialize Executor for transaction simulation and execution
    let executor = Executor::new(
        std::env::var("RPC_URL")
            .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string()), // Ethereum RPC
        Chain::Ethereum,
        0.005, // 0.5% allowed slippage
        router_address,
        UserTransferType::TransferFrom
    );


    println!("Creating UniswapX Gateway and Solver...");
    let uniswapx_gateway = UniswapXGateway::new(uniswap_config)?;

    // Create the complete UniswapX Solver that orchestrates the full pipeline
    // This integrates: UniswapX API ↔ Gateway ↔ Tycho Router ↔ Executor ↔ Blockchain  
    let uniswap_solver = UniswapXSolver::new(
        uniswapx_gateway,
        tycho_solver,
        executor,
        solver_config,
    );


    // Get polling interval from environment variable or use default
    let poll_interval = std::env::var("POLL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30); // Default 30 second intervals

    println!("Starting continuous UniswapX order processing");
    println!("Polling interval: {}s (set POLL_INTERVAL_SECS to change)", poll_interval);
    println!("Processing pipeline: UniswapX API → Gateway → Tycho Router → Executor → Ethereum");
    println!("Press Ctrl+C to stop gracefully");
    println!();

    // Start continuous background polling (production mode)
    let polling_handle = uniswap_solver.start_background_polling(poll_interval);
    
    // Wait for interrupt signal or polling to end
    tokio::select! {
        result = polling_handle => {
            match result {
                Ok(Ok(_)) => println!("Background polling completed successfully"),
                Ok(Err(e)) => println!("Background polling failed: {}", e),
                Err(e) => println!("Background polling task panicked: {}", e),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\nReceived interrupt signal, shutting down gracefully...");
            // TODO: Implement proper shutdown - currently start_background_polling takes ownership
            // Would need to restructure to use Arc<> pattern or return a shutdown handle
            println!("Shutdown complete");
        }
    }

    Ok(())
}
