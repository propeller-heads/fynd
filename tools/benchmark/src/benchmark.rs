//! Load-test subcommand.
//!
//! Sends quote requests to a single solver, measures round-trip time,
//! server solve time, and overhead, then prints statistics and histograms.

use std::{sync::Arc, time::Instant};

use clap::Parser;
use fynd_client::{FyndClient, FyndClientBuilder};
use tracing::info;

use crate::{
    config::{
        BenchmarkConfig, BenchmarkResults, BenchmarkStatistics, ParallelizationMode, TimingStats,
    },
    exporter::{export_results, print_histogram, print_statistics},
    requests::{default_request, load_request_templates, SwapRequest},
    runner::RunnerResults,
};

/// Measure solver latency and throughput under load.
#[derive(Parser, Debug)]
#[command(
    about = "Load-test a Fynd solver with configurable parallelization",
    long_about = "Load-test a Fynd solver with configurable parallelization.\n\n\
        Always build with --release for accurate measurements."
)]
pub struct Args {
    /// Base URL of the solver to benchmark
    #[arg(long, env = "SOLVER_URL", default_value = "http://localhost:3000")]
    pub solver_url: String,

    /// Total number of quote requests to send
    #[arg(long, short = 'n', env = "NUM_REQUESTS", default_value = "1")]
    pub num_requests: usize,

    /// How to schedule requests: "sequential", "fixed:N" (N concurrent),
    /// or "rate:N" (one request every N ms)
    #[arg(long, short = 'm', env = "PARALLELIZATION_MODE", default_value = "sequential")]
    pub parallelization_mode: String,

    /// JSON file of request templates (see requests_set.json for format).
    /// Defaults to a single 1 WETH -> USDC swap.
    #[arg(long, env = "REQUESTS_FILE")]
    pub requests_file: Option<String>,

    /// Write full results (config + all timings) to this JSON file
    #[arg(long, env = "OUTPUT_FILE")]
    pub output_file: Option<String>,
}

/// Execute the load-test: health-check, send requests, print stats.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let parallelization_mode: ParallelizationMode = args
        .parallelization_mode
        .parse()
        .map_err(|e: Box<dyn std::error::Error>| anyhow::anyhow!("{e}"))?;

    info!("Solver URL: {}", args.solver_url);
    info!("Number of requests: {}", args.num_requests);
    info!("Parallelization mode: {:?}", parallelization_mode);

    let client = Arc::new(
        FyndClientBuilder::new(&args.solver_url)
            .build_quote_only()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );

    check_solver_health(&client).await?;
    info!("Solver is ready");

    let (requests, requests_file) = load_requests(args.requests_file.as_deref())?;

    let benchmark_start = Instant::now();
    let RunnerResults {
        round_trip_times,
        solve_times,
        successful_requests,
        orders_solved,
        orders_unsolved,
    } = parallelization_mode
        .run(Arc::clone(&client), &requests, args.num_requests)
        .await;
    let total_duration_ms = benchmark_start.elapsed().as_millis() as u64;

    let overheads: Vec<u64> = round_trip_times
        .iter()
        .zip(solve_times.iter())
        .map(|(rt, st)| rt.saturating_sub(*st))
        .collect();

    if successful_requests > 0 {
        let failed_requests = args.num_requests - successful_requests;
        let throughput_rps = if total_duration_ms > 0 {
            (successful_requests as f64 * 1000.0) / total_duration_ms as f64
        } else {
            0.0
        };

        println!("\n=== Results ===");
        println!("Successful HTTP requests: {}/{}", successful_requests, args.num_requests);
        println!("Failed HTTP requests:     {}", failed_requests);
        println!("Orders solved:            {}", orders_solved);
        println!("Orders not solved:        {}", orders_unsolved);
        println!("Total duration:      {:.2}s", total_duration_ms as f64 / 1000.0);
        println!("Throughput:          {:.2} req/s", throughput_rps);

        print_statistics(&round_trip_times, "Round-trip times (client → server → client):");
        print_histogram(&round_trip_times, "Round-trip", 50);

        print_statistics(&solve_times, "Server solve times (WorkerPoolRouter timing):");
        print_histogram(&solve_times, "Solve time", 50);

        print_statistics(&overheads, "Overhead (round-trip - solve time):");
        print_histogram(&overheads, "Overhead", 50);

        if let Some(output_file) = args.output_file {
            let config = BenchmarkConfig {
                solver_url: args.solver_url.clone(),
                num_requests: args.num_requests,
                parallelization_mode,
                requests_file,
                num_request_templates: requests.len(),
            };

            let statistics = BenchmarkStatistics {
                round_trip: TimingStats::from_measurements(&round_trip_times).unwrap(),
                solve_time: TimingStats::from_measurements(&solve_times).unwrap(),
                overhead: TimingStats::from_measurements(&overheads).unwrap(),
            };
            let results = BenchmarkResults {
                config,
                request_templates: requests,
                successful_requests,
                failed_requests,
                orders_solved,
                orders_unsolved,
                total_duration_ms,
                throughput_rps,
                round_trip_times_ms: round_trip_times,
                solve_times_ms: solve_times,
                overhead_times_ms: overheads,
                statistics,
            };

            export_results(results, output_file).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    } else {
        tracing::warn!("No successful requests!");
    }

    Ok(())
}

fn load_requests(
    requests_file: Option<&str>,
) -> anyhow::Result<(Vec<SwapRequest>, Option<String>)> {
    let requests = if let Some(file_path) = requests_file {
        info!("Loading requests from: {}", file_path);
        let loaded =
            load_request_templates(file_path, 10000).map_err(|e| anyhow::anyhow!("{e}"))?;
        info!("Loaded {} request template(s)", loaded.len());
        loaded
    } else {
        info!("No requests file specified, using default request template");
        vec![default_request(10000)]
    };

    if requests.len() == 1 {
        println!("Request template: {}", requests[0].label);
    } else {
        println!("Using {} different request templates (randomized)", requests.len());
    }
    println!();

    Ok((requests, requests_file.map(|s| s.to_string())))
}

async fn check_solver_health(client: &FyndClient) -> anyhow::Result<()> {
    info!("Checking solver health...");

    let health = client
        .health()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !health.healthy() {
        return Err(anyhow::anyhow!("Solver is not healthy"));
    }

    info!(
        "Market data age: {}ms, Solver pools: {}",
        health.last_update_ms(),
        health.num_solver_pools()
    );

    Ok(())
}
