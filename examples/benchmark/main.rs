mod config;
mod exporter;
mod runner;

use std::time::Instant;

use clap::Parser;
use config::{load_requests, BenchmarkConfig, BenchmarkResults, ParallelizationMode};
use exporter::{export_results, print_histogram, print_statistics};
use fynd_rpc::HealthStatus;
use runner::{run_benchmark, RunnerResults};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Benchmark tool for measuring fynd's performance
#[derive(Parser, Debug)]
#[command(name = "benchmark")]
#[command(about = "Benchmark fynd with various parallelization strategies", long_about = None)]
struct Cli {
    /// Solver URL to benchmark against
    #[arg(long, env = "SOLVER_URL", default_value = "http://localhost:3000")]
    solver_url: String,

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

    let parallelization_mode = ParallelizationMode::from_str(&cli.parallelization_mode)?;

    // Print configuration
    info!("Solver URL: {}", cli.solver_url);
    info!("Number of requests: {}", cli.num_requests);
    info!("Parallelization mode: {:?}", parallelization_mode);

    // Check if solver is ready
    check_solver_health(&cli.solver_url).await?;
    info!("Solver is ready");

    // Load requests
    let (requests, requests_file) = load_requests(cli.requests_file.as_deref())?;

    // Run benchmark
    let client = reqwest::Client::new();
    let benchmark_start = Instant::now();
    let RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_found: orders_solved,
        orders_not_found: orders_not_solved,
    } = run_benchmark(client, &cli.solver_url, &requests, cli.num_requests, &parallelization_mode)
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
        println!("Successful HTTP requests: {}/{}", successful_requests, cli.num_requests);
        println!("Failed HTTP requests:     {}", failed_requests);
        println!("Orders solved:            {}", orders_solved);
        println!("Orders not solved:        {}", orders_not_solved);
        println!("Total duration:      {:.2}s", total_duration_ms as f64 / 1000.0);
        println!("Throughput:          {:.2} req/s", throughput_rps);

        print_statistics(&round_trip_times, "Round-trip times (client → server → client):");
        print_histogram(&round_trip_times, "Round-trip", 50);

        print_statistics(&solve_times, "Server solve times (OrderManager timing):");
        print_histogram(&solve_times, "Solve time", 50);

        print_statistics(&overheads, "Overhead (round-trip - solve time):");
        print_histogram(&overheads, "Overhead", 50);

        if let Some(output_file) = cli.output_file {
            let config = BenchmarkConfig {
                solver_url: cli.solver_url.clone(),
                num_requests: cli.num_requests,
                parallelization_mode,
                requests_file,
                num_request_templates: requests.len(),
                chain: None,
                rpc_url: None,
                tycho_url: None,
                protocols: Vec::new(),
                worker_pools_config_path: None,
                worker_pools_config: None,
            };

            let results = BenchmarkResults::new(
                config,
                requests,
                successful_requests,
                failed_requests,
                orders_solved,
                orders_not_solved,
                total_duration_ms,
                throughput_rps,
                round_trip_times,
                solve_times,
                overheads,
            );

            export_results(results, output_file)?;
        }
    } else {
        tracing::warn!("No successful requests!");
    }

    Ok(())
}

async fn check_solver_health(solver_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let health_url = format!("{}/v1/health", solver_url);

    info!("Checking solver health...");

    let resp = client.get(&health_url).send().await?;
    if !resp.status().is_success() {
        return Err(format!("Solver health check failed with status: {}", resp.status()).into());
    }

    let health = resp.json::<HealthStatus>().await?;
    if !health.healthy {
        return Err("Solver is not healthy".into());
    }

    info!(
        "Market data age: {}ms, Solver pools: {}",
        health.last_update_ms, health.num_solver_pools
    );

    Ok(())
}
