use std::str::FromStr;

use fynd_core::{Order, OrderSide, SolutionOptions, SolutionRequest};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;

/// Parallelization mode for benchmark execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParallelizationMode {
    Sequential,
    #[serde(rename = "fixed_concurrency")]
    FixedConcurrency {
        concurrency: usize,
    },
    #[serde(rename = "rate_based")]
    RateBased {
        interval_ms: u64,
    },
}

impl ParallelizationMode {
    /// Parse parallelization mode from string (e.g., "sequential", "fixed:5", "rate:100")
    pub fn from_str(mode_str: &str) -> Result<Self, Box<dyn std::error::Error>> {
        if mode_str == "sequential" {
            return Ok(Self::Sequential);
        }

        if let Some(concurrency_str) = mode_str.strip_prefix("fixed:") {
            let concurrency = concurrency_str.parse::<usize>()?;
            if concurrency == 0 {
                return Err("Fixed concurrency must be at least 1".into());
            }
            return Ok(Self::FixedConcurrency { concurrency });
        }

        if let Some(interval_str) = mode_str.strip_prefix("rate:") {
            let interval_ms = interval_str.parse::<u64>()?;
            if interval_ms == 0 {
                return Err("Rate interval must be at least 1ms".into());
            }
            return Ok(Self::RateBased { interval_ms });
        }

        Err(format!(
            "Invalid parallelization mode: '{}'. Expected 'sequential', 'fixed:N', or 'rate:Nms'",
            mode_str
        )
        .into())
    }
}

#[derive(Debug, Serialize)]
pub struct BenchmarkConfig {
    pub solver_url: String,
    pub num_requests: usize,
    pub parallelization_mode: ParallelizationMode,
    pub requests_file: Option<String>,
    pub num_request_templates: usize,
    // Optional solver metadata (may not be available when benchmarking remote solvers)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tycho_url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_pools_config_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_pools_config: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkResults {
    pub config: BenchmarkConfig,
    pub request_templates: Vec<SolutionRequest>,
    pub successful_requests: usize,
    pub failed_requests: usize,
    pub orders_solved: usize,
    pub orders_not_solved: usize,
    pub total_duration_ms: u64,
    pub throughput_rps: f64,
    pub round_trip_times_ms: Vec<u64>,
    pub solve_times_ms: Vec<u64>,
    pub overhead_times_ms: Vec<u64>,
    pub statistics: BenchmarkStatistics,
}

impl BenchmarkResults {
    /// Creates a new BenchmarkResults from configuration and raw measurements.
    /// Calculates statistics automatically from the timing data.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: BenchmarkConfig,
        requests: Vec<SolutionRequest>,
        successful_requests: usize,
        failed_requests: usize,
        orders_solved: usize,
        orders_not_solved: usize,
        total_duration_ms: u64,
        throughput_rps: f64,
        round_trip_times: Vec<u64>,
        solve_times: Vec<u64>,
        overheads: Vec<u64>,
    ) -> Self {
        let round_trip_stats = TimingStats::from_measurements(&round_trip_times).unwrap();
        let solve_time_stats = TimingStats::from_measurements(&solve_times).unwrap();
        let overhead_stats = TimingStats::from_measurements(&overheads).unwrap();

        Self {
            config,
            request_templates: requests,
            successful_requests,
            failed_requests,
            orders_solved,
            orders_not_solved,
            total_duration_ms,
            throughput_rps,
            round_trip_times_ms: round_trip_times,
            solve_times_ms: solve_times,
            overhead_times_ms: overheads,
            statistics: BenchmarkStatistics {
                round_trip: round_trip_stats,
                solve_time: solve_time_stats,
                overhead: overhead_stats,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BenchmarkStatistics {
    pub round_trip: TimingStats,
    pub solve_time: TimingStats,
    pub overhead: TimingStats,
}

#[derive(Debug, Serialize)]
pub struct TimingStats {
    pub min: u64,
    pub max: u64,
    pub mean: u64,
    pub median: u64,
    pub p95: u64,
    pub p99: u64,
    pub std_dev: f64,
}

/// Load request templates from file or use default
pub fn load_requests(
    requests_file: Option<&str>,
) -> Result<(Vec<SolutionRequest>, Option<String>), Box<dyn std::error::Error>> {
    let requests = if let Some(file_path) = requests_file {
        tracing::info!("Loading requests from: {}", file_path);
        let content = std::fs::read_to_string(file_path)?;
        let loaded_requests: Vec<SolutionRequest> = serde_json::from_str(&content)?;
        if loaded_requests.is_empty() {
            return Err("Requests file contains no requests".into());
        }
        tracing::info!("Loaded {} request template(s)", loaded_requests.len());
        loaded_requests
    } else {
        tracing::info!("No requests file specified, using default request template");
        vec![create_default_request()?]
    };

    if requests.len() == 1 {
        println!("Request template:");
        println!("{}", serde_json::to_string_pretty(&requests[0])?);
    } else {
        println!("Using {} different request templates (randomized)", requests.len());
        println!("First template example:");
        println!("{}", serde_json::to_string_pretty(&requests[0])?);
    }
    println!();

    Ok((requests, requests_file.map(|s| s.to_string())))
}

fn create_default_request() -> Result<SolutionRequest, Box<dyn std::error::Error>> {
    Ok(SolutionRequest {
        orders: vec![Order {
            id: String::new(),
            token_in: Address::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?, // WETH
            token_out: Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?, // USDC
            amount: BigUint::from_str("1000000000000000000")?,                          // 1 WETH
            side: OrderSide::Sell,
            sender: Address::from_str("0x0000000000000000000000000000000000000001")?,
            receiver: None,
        }],
        options: SolutionOptions { timeout_ms: Some(10000), ..Default::default() },
    })
}
