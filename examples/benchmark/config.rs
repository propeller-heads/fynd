use std::str::FromStr;

use alloy::hex;
use bytes::Bytes;
use fynd_client::{Order, OrderSide, QuoteOptions, QuoteParams};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

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

/// A benchmark request template, serializable to/from the standard wire JSON format.
///
/// Each template is converted to a [`QuoteParams`] when passed to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestTemplate {
    orders: Vec<OrderTemplate>,
    options: OptionsTemplate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderTemplate {
    #[serde(default)]
    id: String,
    token_in: String,
    token_out: String,
    amount: String,
    side: String,
    sender: String,
    receiver: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OptionsTemplate {
    timeout_ms: Option<u64>,
    min_responses: Option<usize>,
    max_gas: Option<String>,
}

fn parse_address(hex: &str) -> Result<Bytes, Box<dyn std::error::Error>> {
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    let raw = hex::decode(hex)?;
    if raw.len() != 20 {
        return Err(format!("expected 20-byte address, got {} bytes", raw.len()).into());
    }
    Ok(Bytes::from(raw))
}

impl RequestTemplate {
    pub fn to_quote_params(&self) -> Result<QuoteParams, Box<dyn std::error::Error>> {
        let order_tmpl = self
            .orders
            .first()
            .ok_or("request template has no orders")?;

        let token_in = parse_address(&order_tmpl.token_in)?;
        let token_out = parse_address(&order_tmpl.token_out)?;
        let sender = parse_address(&order_tmpl.sender)?;
        let receiver = order_tmpl
            .receiver
            .as_deref()
            .map(parse_address)
            .transpose()?;
        let amount = BigUint::from_str(&order_tmpl.amount)?;

        let side = match order_tmpl.side.as_str() {
            "sell" => OrderSide::Sell,
            other => return Err(format!("unsupported order side: {other}").into()),
        };

        let mut options = QuoteOptions::default();
        if let Some(ms) = self.options.timeout_ms {
            options = options.with_timeout_ms(ms);
        }
        if let Some(n) = self.options.min_responses {
            options = options.with_min_responses(n);
        }
        if let Some(ref gas_str) = self.options.max_gas {
            options = options.with_max_gas(BigUint::from_str(gas_str)?);
        }

        let order = Order::new(token_in, token_out, amount, side, sender, receiver);
        Ok(QuoteParams::new(order, options))
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
    pub request_templates: Vec<RequestTemplate>,
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
        requests: Vec<RequestTemplate>,
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

/// Load request templates from file or use the built-in default.
///
/// Validates all templates by converting them to [`QuoteParams`] — returns an error
/// if any template contains an invalid address or unsupported order side.
pub fn load_requests(
    requests_file: Option<&str>,
) -> Result<(Vec<RequestTemplate>, Option<String>), Box<dyn std::error::Error>> {
    let requests = if let Some(file_path) = requests_file {
        tracing::info!("Loading requests from: {}", file_path);
        let content = std::fs::read_to_string(file_path)?;
        let loaded: Vec<RequestTemplate> = serde_json::from_str(&content)?;
        if loaded.is_empty() {
            return Err("Requests file contains no requests".into());
        }
        tracing::info!("Loaded {} request template(s)", loaded.len());
        loaded
    } else {
        tracing::info!("No requests file specified, using default request template");
        vec![create_default_request()]
    };

    // Validate all templates up-front so we fail early rather than mid-benchmark.
    for (i, tmpl) in requests.iter().enumerate() {
        tmpl.to_quote_params()
            .map_err(|e| format!("request template {i} is invalid: {e}"))?;
    }

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

fn create_default_request() -> RequestTemplate {
    RequestTemplate {
        orders: vec![OrderTemplate {
            id: String::new(),
            token_in: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(), // WETH
            token_out: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(), // USDC
            amount: "1000000000000000000".to_string(),                          // 1 WETH
            side: "sell".to_string(),
            sender: "0x0000000000000000000000000000000000000001".to_string(),
            receiver: None,
        }],
        options: OptionsTemplate { timeout_ms: Some(10000), min_responses: None, max_gas: None },
    }
}
