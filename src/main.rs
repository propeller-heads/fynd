use std::env;
use tycho_router::{
    api::RouterApi,
    modules::{algorithm::most_liquid::MostLiquidAlgorithm, execution::executor::Executor},
    server::RouterServer,
    solver::Solver,
};
use tycho_simulation::tycho_common::models::Chain;
use tycho_execution::encoding::models::UserTransferType;

/// Tycho Router Server
/// 
/// A high-performance routing engine that finds optimal paths through DEX liquidity
/// and provides HTTP API endpoints for quote and execution services.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("🚀 Starting Tycho Router Server");
    println!("================================");

    // Load configuration from environment variables
    let tycho_url = env::var("TYCHO_URL")
        .unwrap_or_else(|_| "wss://api.tycho.org".to_string());
    
    let tycho_api_key = env::var("TYCHO_API_KEY")
        .unwrap_or_else(|_| "demo_key".to_string());
    
    let rpc_url = env::var("RPC_URL")
        .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string());
    
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    
    let max_hops: usize = env::var("MAX_HOPS")
        .ok()
        .and_then(|h| h.parse().ok())
        .unwrap_or(3);
    
    let min_tvl: f64 = env::var("MIN_TVL")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(1000.0);
    
    let max_tvl: f64 = env::var("MAX_TVL")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(1_000_000.0);

    let router_address = env::var("ROUTER_ADDRESS")
        .unwrap_or_else(|_| "0x1234567890123456789012345678901234567890".to_string());

    println!("Configuration:");
    println!("  🌐 Chain: Ethereum");
    println!("  🔗 Tycho URL: {}", tycho_url);
    println!("  🔑 API Key: {}", if tycho_api_key == "demo_key" { "demo_key (rate limited)" } else { "✅ configured" });
    println!("  ⛽ RPC URL: {}", rpc_url);
    println!("  📡 Port: {}", port);
    println!("  🔄 Max Hops: {}", max_hops);
    println!("  💰 TVL Range: ${:.0} - ${:.0}", min_tvl, max_tvl);
    println!();

    println!("🔧 Initializing solver...");
    
    // 1. Create solver with MostLiquidAlgorithm
    let solver = Solver::<MostLiquidAlgorithm>::new(
        max_hops,
        Chain::Ethereum,
        tycho_url,
        tycho_api_key,
        Some(vec![
            "uniswap_v2".to_string(),
            "uniswap_v3".to_string(),
            "sushiswap".to_string(),
            "curve".to_string(),
            "balancer".to_string(),
        ]),
        None, // Load all tokens from Tycho
        (min_tvl, max_tvl),
    ).await?;

    println!("✅ Solver initialized with MostLiquidAlgorithm");

    // 2. Start background market data updates
    println!("🔄 Starting background market data updates...");
    let (_indexer_handle, _gas_handle) = solver.start_background_updates().await?;
    println!("✅ Background updates started (indexer & gas price fetcher)");

    // 3. Create executor for transaction handling
    println!("⚙️  Setting up transaction executor...");
    let executor = Executor::new(
        rpc_url,
        Chain::Ethereum,
        0.005, // 0.5% slippage tolerance
        tycho_simulation::tycho_common::Bytes::from(router_address.as_str()),
        UserTransferType::TransferFrom,
    );
    println!("✅ Executor configured");

    // 4. Create API and server
    println!("🌐 Setting up HTTP API...");
    let api = RouterApi::new(solver, executor);
    let server = RouterServer::new(api, port);

    println!("✅ API configured");
    println!();
    
    // 5. Start the server
    println!("🎯 Server starting on http://localhost:{}", port);
    println!("📍 Available endpoints:");
    println!("   POST /solve           - Get routes + encoded transactions (no execution)");
    println!("   POST /solve_and_execute - Get routes + transactions + execute swaps");
    println!("   POST /track           - Track transaction status by hash");
    println!("   GET  /health          - Health check");
    println!();
    println!("💡 Example usage:");
    println!("   curl http://localhost:{}/health", port);
    println!("   curl -X POST http://localhost:{}/solve -H 'Content-Type: application/json' -d '[{{...order...}}]'", port);
    println!();
    println!("🔍 Background services:");
    println!("   📊 Market data indexer (Tycho WebSocket)");
    println!("   ⛽ Gas price fetcher (30s intervals)");
    println!("   🧠 Route optimization (MostLiquidAlgorithm)");
    println!();
    println!("Press Ctrl+C to stop gracefully");
    println!("=====================================");

    // Run the server (this blocks until shutdown)
    server.run().await?;

    println!("👋 Tycho Router Server stopped gracefully");
    Ok(())
}