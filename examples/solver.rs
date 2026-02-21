use clap::Parser;
use fynd::{parse_chain, FyndBuilder, HealthStatus, WorkerPoolsConfig};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Fynd HTTP server
#[derive(Parser, Debug)]
#[command(name = "solver")]
#[command(about = "Run Fynd HTTP server", long_about = None)]
struct Cli {
    /// RPC endpoint URL
    #[arg(long, env = "RPC_URL")]
    rpc_url: String,

    /// Tycho indexer URL
    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    /// Blockchain network
    #[arg(long, env = "CHAIN", default_value = "Ethereum")]
    chain: String,

    /// Comma-separated protocol list
    #[arg(long, env = "PROTOCOLS", default_value = "uniswap_v2,uniswap_v3")]
    protocols: String,

    /// HTTP server port
    #[arg(long, env = "HTTP_PORT", default_value = "3000")]
    http_port: u16,

    /// Worker pool configuration file
    #[arg(long, env = "WORKER_POOLS_CONFIG", default_value = "worker_pools.toml")]
    worker_pools_config: String,

    /// Tycho API key
    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: Option<String>,

    /// Minimum TVL threshold in native token (e.g. ETH)
    #[arg(long, env = "MIN_TVL", default_value = "10.0")]
    min_tvl: f64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("RUST_LOG"))
        .with_target(true)
        .init();

    // Parse CLI arguments
    let cli = Cli::parse();

    let chain = parse_chain(&cli.chain)?;
    let protocols: Vec<String> = cli
        .protocols
        .split(',')
        .map(|s| s.to_string())
        .collect();

    // Print configuration
    info!("Chain: {}", cli.chain);
    info!("Tycho URL: {}", cli.tycho_url);
    info!("Protocols: {}", cli.protocols);
    info!("HTTP Port: {}", cli.http_port);
    info!("Worker pools config: {}", cli.worker_pools_config);
    info!("Min TVL: {}", cli.min_tvl);

    // Load worker pools configuration
    let pools_config = WorkerPoolsConfig::load_from_file(&cli.worker_pools_config)?;
    info!("Loaded {} worker pool(s)", pools_config.pools.len());

    // Build and spawn the solver
    info!("Starting fynd...");
    let mut builder = FyndBuilder::new(
        chain,
        pools_config.pools,
        cli.tycho_url.clone(),
        cli.rpc_url.clone(),
        protocols,
    )
    .http_port(cli.http_port);

    builder = builder.min_tvl(cli.min_tvl);

    if let Some(api_key) = cli.tycho_api_key {
        builder = builder.tycho_api_key(api_key);
    }

    let solver = builder.build()?;

    let server_handle = solver.server_handle();
    let mut solver_task = tokio::spawn(async move {
        if let Err(e) = solver.run().await {
            error!("Solver error: {}", e);
            Err(e.to_string())
        } else {
            Ok(())
        }
    });

    // Wait for solver to be ready, or for early exit/Ctrl+C
    let solver_url = format!("http://localhost:{}", cli.http_port);
    tokio::select! {
        result = wait_for_solver_ready(&solver_url) => {
            match result {
                Ok(_) => info!("Solver is ready and accepting requests at {}", solver_url),
                Err(e) => {
                    error!("Failed to start solver: {}", e);
                    server_handle.stop(true).await;
                    let _ = solver_task.await;
                    return Err(e);
                }
            }
        }
        result = &mut solver_task => {
            // Solver exited early (probably an error during startup)
            server_handle.stop(true).await;
            match result {
                Ok(Ok(_)) => return Err("Solver exited unexpectedly during startup".into()),
                Ok(Err(e)) => return Err(e.into()),
                Err(e) => return Err(format!("Solver task panicked: {}", e).into()),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C during startup, shutting down...");
            server_handle.stop(true).await;
            let _ = solver_task.await;
            return Ok(());
        }
    }

    // Keep running until interrupted or solver exits
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        result = solver_task => {
            match result {
                Ok(Ok(_)) => info!("Solver exited cleanly"),
                Ok(Err(e)) => error!("Solver error: {}", e),
                Err(e) => error!("Solver task panicked: {}", e),
            }
        }
    }

    // Shutdown
    info!("Shutting down solver...");
    server_handle.stop(true).await;
    info!("Solver stopped");

    Ok(())
}

async fn wait_for_solver_ready(solver_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let health_url = format!("{}/v1/health", solver_url);
    let max_attempts = 120;
    let mut attempts = 0;

    info!("Waiting for market data to load...");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    loop {
        attempts += 1;
        if attempts > max_attempts {
            return Err(
                "Solver failed to become ready within timeout. Market data may not have loaded."
                    .into(),
            );
        }

        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(health) = resp.json::<HealthStatus>().await {
                    if health.healthy {
                        info!(
                            "Market data age: {}ms, Solver pools: {}",
                            health.last_update_ms, health.num_solver_pools
                        );
                        return Ok(());
                    }
                }
            }
            _ => {}
        }

        std::io::Write::flush(&mut std::io::stdout()).ok();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}
