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

impl std::str::FromStr for ParallelizationMode {
    type Err = Box<dyn std::error::Error>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "sequential" {
            return Ok(Self::Sequential);
        }

        if let Some(concurrency_str) = s.strip_prefix("fixed:") {
            let concurrency = concurrency_str.parse::<usize>()?;
            if concurrency == 0 {
                return Err("Fixed concurrency must be at least 1".into());
            }
            return Ok(Self::FixedConcurrency { concurrency });
        }

        if let Some(interval_str) = s.strip_prefix("rate:") {
            let interval_ms = interval_str.parse::<u64>()?;
            if interval_ms == 0 {
                return Err("Rate interval must be at least 1ms".into());
            }
            return Ok(Self::RateBased { interval_ms });
        }

        Err(format!(
            "Invalid parallelization mode: '{s}'. Expected 'sequential', 'fixed:N', or 'rate:Nms'"
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

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
    pub orders_unsolved: usize,
    pub total_duration_ms: u64,
    pub throughput_rps: f64,
    pub round_trip_times_ms: Vec<u64>,
    pub solve_times_ms: Vec<u64>,
    pub overhead_times_ms: Vec<u64>,
    pub statistics: BenchmarkStatistics,
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
