//! Shared configuration and statistics types for the load-test subcommand.

use serde::Serialize;

use crate::requests::SwapRequest;

#[derive(Debug, Clone, Serialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential() {
        let mode = ParallelizationMode::from_str("sequential").unwrap();
        assert!(matches!(mode, ParallelizationMode::Sequential));
    }

    #[test]
    fn fixed_valid() {
        let mode = ParallelizationMode::from_str("fixed:4").unwrap();
        assert!(matches!(mode, ParallelizationMode::FixedConcurrency { concurrency: 4 }));
    }

    #[test]
    fn fixed_one() {
        let mode = ParallelizationMode::from_str("fixed:1").unwrap();
        assert!(matches!(mode, ParallelizationMode::FixedConcurrency { concurrency: 1 }));
    }

    #[test]
    fn fixed_zero() {
        let err = ParallelizationMode::from_str("fixed:0").unwrap_err();
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn fixed_non_numeric() {
        assert!(ParallelizationMode::from_str("fixed:abc").is_err());
    }

    #[test]
    fn fixed_empty() {
        assert!(ParallelizationMode::from_str("fixed:").is_err());
    }

    #[test]
    fn rate_valid() {
        let mode = ParallelizationMode::from_str("rate:100").unwrap();
        assert!(matches!(mode, ParallelizationMode::RateBased { interval_ms: 100 }));
    }

    #[test]
    fn rate_one() {
        let mode = ParallelizationMode::from_str("rate:1").unwrap();
        assert!(matches!(mode, ParallelizationMode::RateBased { interval_ms: 1 }));
    }

    #[test]
    fn rate_zero() {
        let err = ParallelizationMode::from_str("rate:0").unwrap_err();
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn rate_non_numeric() {
        assert!(ParallelizationMode::from_str("rate:abc").is_err());
    }

    #[test]
    fn invalid_mode() {
        let err = ParallelizationMode::from_str("invalid").unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid parallelization mode"));
    }

    #[test]
    fn empty_string() {
        assert!(ParallelizationMode::from_str("").is_err());
    }
}

#[derive(Debug, Serialize)]
pub struct BenchmarkConfig {
    pub solver_url: String,
    pub num_requests: usize,
    pub parallelization_mode: ParallelizationMode,
    pub requests_file: Option<String>,
    pub num_request_templates: usize,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkResults {
    pub config: BenchmarkConfig,
    pub request_templates: Vec<SwapRequest>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: BenchmarkConfig,
        requests: Vec<SwapRequest>,
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
