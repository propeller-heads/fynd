mod config;
mod exporter;
mod runner;

use std::time::Instant;

use clap::Parser;
use config::{load_requests, ParallelizationMode};
use exporter::{export_results, print_histogram, print_statistics};
use runner::run_benchmark;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tycho_solver::{parse_chain, HealthStatus, TychoSolverBuilder, WorkerPoolsConfig};

/// Benchmark tool for measuring tycho-solver performance
#[derive(Parser, Debug)]
#[command(name = "benchmark")]
#[command(about = "Benchmark tycho-solver with various parallelization strategies", long_about = None)]
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

    /// Number of requests to benchmark
    #[arg(long, short = 'n', env = "NUM_REQUESTS", default_value = "1")]
    num_requests: usize,

    /// Parallelization mode: sequential, fixed:N, or rate:Nms
    #[arg(long, short = 'm', env = "PARALLELIZATION_MODE", default_value = "sequential")]
    parallelization_mode: String,

    /// Path to JSON file with request templates
    #[arg(long, env = "REQUESTS_FILE")]
    requests_file: Option<String>,

    /// Output file for results (if not specified, results are not exported to file)
    #[arg(long, env = "OUTPUT_FILE")]
    output_file: Option<String>,
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
    let parallelization_mode = ParallelizationMode::from_str(&cli.parallelization_mode)?;

    // Print configuration
    info!("Chain: {}", cli.chain);
    info!("Tycho URL: {}", cli.tycho_url);
    info!("Protocols: {}", cli.protocols);
    info!("HTTP Port: {}", cli.http_port);
    info!("Worker pools config: {}", cli.worker_pools_config);
    info!("Number of requests: {}", cli.num_requests);
    info!("Parallelization mode: {:?}", parallelization_mode);

    // Load worker pools configuration
    let pools_config = WorkerPoolsConfig::load_from_file(&cli.worker_pools_config)?;
    let worker_pools_config_content = std::fs::read_to_string(&cli.worker_pools_config)?;
    info!("Loaded {} worker pool(s)", pools_config.pools.len());

    // Build and spawn the solver
    info!("Starting tycho-solver...");
    let solver = TychoSolverBuilder::new(
        chain,
        pools_config.pools,
        cli.tycho_url.clone(),
        cli.rpc_url.clone(),
        protocols.clone(),
    )
    .http_port(cli.http_port)
    .build()?;

    let server_handle = solver.server_handle();
    let solver_task = tokio::spawn(async move {
        if let Err(e) = solver.run().await {
            error!("Solver error: {}", e);
        }
    });

    // Wait for solver to be ready
    let solver_url = format!("http://localhost:{}", cli.http_port);
    if let Err(e) = wait_for_solver_ready(&solver_url).await {
        error!("Failed to start solver: {}", e);
        server_handle.stop(true).await;
        return Err(e);
    }
    info!("Solver is ready");

    // Load requests
    let (requests, requests_file) = load_requests(cli.requests_file.as_deref())?;

    // Run benchmark
    let client = reqwest::Client::new();
    let benchmark_start = Instant::now();
    let (round_trip_times, solve_times, successful_requests) =
        run_benchmark(client, &solver_url, &requests, cli.num_requests, &parallelization_mode)
            .await;
    let total_duration_ms = benchmark_start.elapsed().as_millis() as u64;

    // Calculate overhead
    let overheads: Vec<u64> = round_trip_times
        .iter()
        .zip(solve_times.iter())
        .map(|(rt, st)| rt.saturating_sub(*st))
        .collect();

    // Print and export results
    if successful_requests > 0 {
        let failed_requests = cli.num_requests - successful_requests;
        let throughput_rps = if total_duration_ms > 0 {
            (successful_requests as f64 * 1000.0) / total_duration_ms as f64
        } else {
            0.0
        };

        println!("\n=== Results ===");
        println!("Successful requests: {}/{}", successful_requests, cli.num_requests);
        println!("Failed requests:     {}", failed_requests);
        println!("Total duration:      {:.2}s", total_duration_ms as f64 / 1000.0);
        println!("Throughput:          {:.2} req/s", throughput_rps);

        print_statistics(&round_trip_times, "Round-trip times (client → server → client):");
        print_histogram(&round_trip_times, "Round-trip", 50);

        print_statistics(&solve_times, "Server solve times (OrderManager timing):");
        print_histogram(&solve_times, "Solve time", 50);

        print_statistics(&overheads, "Overhead (round-trip - solve time):");
        print_histogram(&overheads, "Overhead", 50);

        if let Some(output_file) = cli.output_file {
            export_results(
                cli.chain,
                cli.rpc_url,
                cli.tycho_url,
                protocols,
                cli.http_port,
                cli.num_requests,
                parallelization_mode,
                cli.worker_pools_config,
                worker_pools_config_content,
                output_file,
                requests_file,
                requests,
                successful_requests,
                failed_requests,
                total_duration_ms,
                throughput_rps,
                round_trip_times,
                solve_times,
                overheads,
            )?;
        }
    } else {
        tracing::warn!("No successful requests!");
    }

    // Shutdown
    tracing::info!("Shutting down solver...");
    server_handle.stop(true).await;
    solver_task.await?;
    tracing::info!("Solver stopped");

    Ok(())
}

async fn wait_for_solver_ready(solver_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let health_url = format!("{}/v1/health", solver_url);
    let max_attempts = 120;
    let mut attempts = 0;

    info!("Waiting for market data to load");
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
                        tracing::info!(
                            "Market data age: {}ms, Solver pools: {}",
                            health.last_update_ms,
                            health.num_solver_pools
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
