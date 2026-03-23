//! CPU scaling subcommand.
//!
//! Measures how solver throughput scales with worker thread count by running
//! repeated load tests across different `num_workers` values, building and
//! tearing down the solver in-process for each iteration.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use fynd_client::FyndClientBuilder;
use fynd_rpc::{
    builder::{FyndRPC, FyndRPCBuilder},
    config::{PoolConfig, WorkerPoolsConfig},
    parse_chain,
};
use serde::Serialize;
use tracing::info;

use crate::{
    config::{ParallelizationMode, TimingStats},
    requests::{default_request, load_request_templates, SwapRequest},
    runner::RunnerResults,
};

/// Measure how solver throughput scales with worker thread count.
#[derive(Parser, Debug)]
#[command(
    about = "Benchmark throughput scaling across different worker counts",
    long_about = "Benchmark throughput scaling across different worker counts.\n\n\
        Builds a solver in-process for each worker count, runs a load test,\n\
        then shuts down and repeats. Requires a single-pool worker_pools.toml."
)]
pub struct Args {
    /// Path to a single-pool worker_pools.toml config
    #[arg(long, default_value = "worker_pools.toml")]
    pub base_config: PathBuf,

    /// Comma-separated worker counts to test (e.g. "1,2,4,8,16")
    #[arg(long)]
    pub worker_counts: String,

    /// Comma-separated protocols for the solver
    #[arg(long)]
    pub protocols: String,

    /// Tycho WebSocket URL
    #[arg(long, default_value = "localhost:4242")]
    pub tycho_url: String,

    /// Tycho API key
    #[arg(long, env = "TYCHO_API_KEY")]
    pub tycho_api_key: Option<String>,

    /// Disable TLS for Tycho connection
    #[arg(long, default_value_t = false)]
    pub disable_tls: bool,

    /// Node RPC URL
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,

    /// Chain name (e.g. "ethereum")
    #[arg(long, default_value = "ethereum")]
    pub chain: String,

    /// HTTP port for the solver
    #[arg(long, default_value_t = 3000)]
    pub http_port: u16,

    /// Number of requests per iteration
    #[arg(long, short = 'n', default_value_t = 100)]
    pub num_requests: usize,

    /// Parallelization mode for load test (e.g. "fixed:8")
    #[arg(long, short = 'm', default_value = "fixed:8")]
    pub parallelization_mode: String,

    /// JSON file of request templates
    #[arg(long)]
    pub requests_file: Option<String>,

    /// Seconds to sleep after health before benchmarking
    #[arg(long, default_value_t = 30)]
    pub warmup_secs: u64,

    /// Max seconds to wait for solver health
    #[arg(long, default_value_t = 300)]
    pub health_timeout_secs: u64,

    /// Write JSON results to this file
    #[arg(long)]
    pub output_file: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScalePoint {
    total_workers: usize,
    throughput_rps: f64,
    total_duration_ms: u64,
    successful_requests: usize,
    failed_requests: usize,
    round_trip: TimingStats,
    solve_time: TimingStats,
}

#[derive(Debug, Serialize)]
struct ScaleConfig {
    base_config: String,
    worker_counts: Vec<usize>,
    num_requests: usize,
    parallelization_mode: String,
    warmup_secs: u64,
    pool_name: String,
    algorithm: String,
}

#[derive(Debug, Serialize)]
struct ScaleResults {
    config: ScaleConfig,
    points: Vec<ScalePoint>,
}

fn parse_worker_counts(s: &str) -> Result<Vec<usize>> {
    let counts: Vec<usize> = s
        .split(',')
        .map(|v| {
            v.trim()
                .parse::<usize>()
                .with_context(|| format!("invalid worker count '{v}'"))
        })
        .collect::<Result<_>>()?;

    if counts.is_empty() {
        bail!("--worker-counts must contain at least one value");
    }
    for &c in &counts {
        if c == 0 {
            bail!("worker count must be >= 1, got 0");
        }
    }
    Ok(counts)
}

fn validate_single_pool(config: &WorkerPoolsConfig) -> Result<(String, PoolConfig)> {
    if config.pools().is_empty() {
        bail!("base config has no pools; exactly one pool is required");
    }
    if config.pools().len() > 1 {
        bail!("base config has {} pools; exactly one pool is required", config.pools().len());
    }
    let (name, pool) = config
        .pools()
        .iter()
        .next()
        .expect("checked non-empty");
    Ok((name.clone(), pool.clone()))
}

fn build_solver(
    args: &Args,
    pool_name: &str,
    pool_config: &PoolConfig,
    workers: usize,
) -> Result<FyndRPC> {
    let chain = parse_chain(&args.chain)?;
    let protocols: Vec<String> = args
        .protocols
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let rpc_url = args
        .rpc_url
        .clone()
        .unwrap_or_else(|| fynd_rpc::config::defaults::DEFAULT_RPC_URL.to_string());

    let pool = pool_config
        .clone()
        .with_num_workers(workers);

    let pools = HashMap::from([(pool_name.to_string(), pool)]);

    let mut builder = FyndRPCBuilder::new(chain, pools, args.tycho_url.clone(), rpc_url, protocols)
        .http_port(args.http_port);

    if let Some(ref key) = args.tycho_api_key {
        builder = builder.tycho_api_key(key.clone());
    }
    if args.disable_tls {
        builder = builder.disable_tls();
    }

    builder.build()
}

async fn wait_for_health(url: &str, timeout: Duration) -> Result<()> {
    let client = FyndClientBuilder::new(url, "")
        .build_quote_only()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        match client.health().await {
            Ok(h) if h.healthy() => return Ok(()),
            Ok(_) => {}
            Err(e) => {
                info!("Health check not ready: {e}");
            }
        }
        if Instant::now() >= deadline {
            bail!("solver did not become healthy within {}s", timeout.as_secs());
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = tokio::signal::ctrl_c() => {
                bail!("interrupted by ctrl+c");
            }
        }
    }
}

fn load_requests(requests_file: Option<&str>) -> Result<Vec<SwapRequest>> {
    if let Some(file_path) = requests_file {
        info!("Loading requests from: {}", file_path);
        let loaded =
            load_request_templates(file_path, 10000).map_err(|e| anyhow::anyhow!("{e}"))?;
        info!("Loaded {} request template(s)", loaded.len());
        Ok(loaded)
    } else {
        info!("Using default WETH->USDC request template");
        Ok(vec![default_request(10000)])
    }
}

fn print_summary(pool_name: &str, algorithm: &str, args: &Args, points: &[ScalePoint]) {
    println!("\n=== CPU Scaling Results ===\n");
    println!("Pool: {} (algorithm: {})", pool_name, algorithm);
    println!("Requests per run: {}, Mode: {}\n", args.num_requests, args.parallelization_mode);
    println!(
        "{:>8} | {:>18} | {:>14} | {:>11} | {:>10}",
        "Workers", "Throughput (req/s)", "Median RT (ms)", "P99 RT (ms)", "RPS/Worker"
    );
    println!("{:-<8}-+-{:-<18}-+-{:-<14}-+-{:-<11}-+-{:-<10}", "", "", "", "", "");
    for p in points {
        let rps_per_worker =
            if p.total_workers > 0 { p.throughput_rps / p.total_workers as f64 } else { 0.0 };
        println!(
            "{:>8} | {:>18.2} | {:>14} | {:>11} | {:>10.2}",
            p.total_workers,
            p.throughput_rps,
            p.round_trip.median,
            p.round_trip.p99,
            rps_per_worker,
        );
    }
    println!();
}

fn export_results(results: &ScaleResults, path: &str) -> Result<()> {
    let json = serde_json::to_string_pretty(results)?;
    std::fs::write(path, json)?;
    info!("Scale results exported to: {}", path);
    Ok(())
}

pub async fn run(args: Args) -> Result<()> {
    let worker_counts = parse_worker_counts(&args.worker_counts)?;
    let parallelization_mode: ParallelizationMode = args
        .parallelization_mode
        .parse()
        .map_err(|e: Box<dyn std::error::Error>| anyhow::anyhow!("{e}"))?;

    let pools_config = WorkerPoolsConfig::load_from_file(&args.base_config)
        .with_context(|| format!("failed to load base config: {}", args.base_config.display()))?;
    let (pool_name, pool_config) = validate_single_pool(&pools_config)?;

    info!("Pool: {} (algorithm: {})", pool_name, pool_config.algorithm());
    info!("Worker counts: {:?}", worker_counts);
    info!("Requests per run: {}", args.num_requests);

    let requests = load_requests(args.requests_file.as_deref())?;
    let solver_url = format!("http://127.0.0.1:{}", args.http_port);

    let mut points = Vec::new();

    for &workers in &worker_counts {
        println!("\n--- Testing with {} worker(s) ---\n", workers);

        let fynd = build_solver(&args, &pool_name, &pool_config, workers)
            .with_context(|| format!("failed to build solver with {workers} workers"))?;
        let handle = fynd.server_handle();
        let server_task = tokio::spawn(fynd.run());

        let result = tokio::select! {
            r = run_iteration(
                &solver_url, &parallelization_mode, &requests, &args, workers,
            ) => r,
            _ = tokio::signal::ctrl_c() => {
                handle.stop(true).await;
                let _ = server_task.await;
                bail!("interrupted by ctrl+c");
            }
        };

        handle.stop(true).await;
        if let Err(e) = server_task.await {
            tracing::warn!("Server task join error: {e}");
        }

        match result {
            Ok(point) => points.push(point),
            Err(e) => {
                tracing::error!("Iteration with {workers} workers failed: {e}");
            }
        }
    }

    if points.is_empty() {
        bail!("all iterations failed; no results to report");
    }

    print_summary(&pool_name, pool_config.algorithm(), &args, &points);

    if let Some(ref path) = args.output_file {
        let results = ScaleResults {
            config: ScaleConfig {
                base_config: args.base_config.display().to_string(),
                worker_counts,
                num_requests: args.num_requests,
                parallelization_mode: args.parallelization_mode.clone(),
                warmup_secs: args.warmup_secs,
                pool_name: pool_name.clone(),
                algorithm: pool_config.algorithm().to_string(),
            },
            points,
        };
        export_results(&results, path)?;
    }

    Ok(())
}

async fn run_iteration(
    solver_url: &str,
    mode: &ParallelizationMode,
    requests: &[SwapRequest],
    args: &Args,
    workers: usize,
) -> Result<ScalePoint> {
    let timeout = Duration::from_secs(args.health_timeout_secs);
    wait_for_health(solver_url, timeout).await?;

    info!("Solver healthy, warming up for {}s...", args.warmup_secs);
    tokio::time::sleep(Duration::from_secs(args.warmup_secs)).await;

    let client = Arc::new(
        FyndClientBuilder::new(solver_url, "")
            .build_quote_only()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );

    let benchmark_start = Instant::now();
    let RunnerResults { round_trip_times, solve_times, successful_requests, .. } = mode
        .run(Arc::clone(&client), requests, args.num_requests)
        .await;
    let total_duration_ms = benchmark_start.elapsed().as_millis() as u64;

    let failed_requests = args.num_requests - successful_requests;

    if successful_requests == 0 {
        bail!("no successful requests for {workers} workers");
    }

    let throughput_rps = if total_duration_ms > 0 {
        (successful_requests as f64 * 1000.0) / total_duration_ms as f64
    } else {
        0.0
    };

    let round_trip =
        TimingStats::from_measurements(&round_trip_times).context("no round-trip timing data")?;
    let solve_time =
        TimingStats::from_measurements(&solve_times).context("no solve-time timing data")?;

    Ok(ScalePoint {
        total_workers: workers,
        throughput_rps,
        total_duration_ms,
        successful_requests,
        failed_requests,
        round_trip,
        solve_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_worker_counts_valid() {
        let counts = parse_worker_counts("1,2,4,8,16").unwrap();
        assert_eq!(counts, vec![1, 2, 4, 8, 16]);
    }

    #[test]
    fn parse_worker_counts_single() {
        let counts = parse_worker_counts("4").unwrap();
        assert_eq!(counts, vec![4]);
    }

    #[test]
    fn parse_worker_counts_with_spaces() {
        let counts = parse_worker_counts("1, 2, 4").unwrap();
        assert_eq!(counts, vec![1, 2, 4]);
    }

    #[test]
    fn parse_worker_counts_zero_rejected() {
        let err = parse_worker_counts("1,0,4").unwrap_err();
        assert!(err.to_string().contains("must be >= 1"));
    }

    #[test]
    fn parse_worker_counts_non_numeric() {
        assert!(parse_worker_counts("1,abc,4").is_err());
    }

    #[test]
    fn validate_single_pool_empty() {
        let config = WorkerPoolsConfig::new(HashMap::new());
        let err = validate_single_pool(&config).unwrap_err();
        assert!(err.to_string().contains("no pools"));
    }

    #[test]
    fn validate_single_pool_one() {
        let mut pools = HashMap::new();
        pools.insert(
            "test_pool".to_string(),
            PoolConfig::new("most_liquid")
                .with_num_workers(4)
                .with_task_queue_capacity(1000)
                .with_min_hops(1)
                .with_max_hops(3)
                .with_timeout_ms(100),
        );
        let config = WorkerPoolsConfig::new(pools);
        let (name, pool) = validate_single_pool(&config).unwrap();
        assert_eq!(name, "test_pool");
        assert_eq!(pool.algorithm(), "most_liquid");
        assert_eq!(pool.num_workers(), 4);
    }

    #[test]
    fn validate_single_pool_two() {
        let mut pools = HashMap::new();
        pools.insert(
            "a".to_string(),
            PoolConfig::new("most_liquid")
                .with_num_workers(4)
                .with_task_queue_capacity(1000)
                .with_min_hops(1)
                .with_max_hops(3)
                .with_timeout_ms(100),
        );
        pools.insert(
            "b".to_string(),
            PoolConfig::new("dijkstra")
                .with_num_workers(2)
                .with_task_queue_capacity(500)
                .with_min_hops(1)
                .with_max_hops(2)
                .with_timeout_ms(200)
                .with_max_routes(Some(10)),
        );
        let config = WorkerPoolsConfig::new(pools);
        let err = validate_single_pool(&config).unwrap_err();
        assert!(err.to_string().contains("2 pools"));
    }
}
