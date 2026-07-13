//! Report types and pass/fail evaluation for the `capacity` subcommand.
//!
//! A ladder step passes while p95 round-trip stays within the policy multiplier of
//! the unloaded baseline and error rates stay below the policy thresholds.

use serde::Serialize;

use crate::config::TimingStats;

/// RPS ladder parsed from `start:step:max` (e.g. `5:5:200`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderSpec {
    pub start: u64,
    pub step: u64,
    pub max: u64,
}

impl std::str::FromStr for LadderSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        let [start, step, max] = parts.as_slice() else {
            return Err(format!("invalid ladder '{s}': expected start:step:max, e.g. 5:5:200"));
        };
        let parse_rate = |value: &str, what: &str| {
            value
                .parse::<u64>()
                .map_err(|e| format!("invalid ladder {what} '{value}': {e}"))
        };
        let spec = Self {
            start: parse_rate(start, "start")?,
            step: parse_rate(step, "step")?,
            max: parse_rate(max, "max")?,
        };
        if spec.start == 0 || spec.step == 0 {
            return Err(format!("invalid ladder '{s}': start and step must be at least 1"));
        }
        if spec.max < spec.start {
            return Err(format!("invalid ladder '{s}': max must be >= start"));
        }
        if spec.max > 1000 {
            return Err(format!(
                "invalid ladder '{s}': rates above 1000 rps are not supported (1ms interval floor)"
            ));
        }
        Ok(spec)
    }
}

impl LadderSpec {
    /// All target rates in ascending order.
    pub fn rates(&self) -> Vec<u64> {
        let mut rates = Vec::new();
        let mut rate = self.start;
        while rate <= self.max {
            rates.push(rate);
            rate += self.step;
        }
        rates
    }
}

/// Pass/fail thresholds for a ladder step, relative to the baseline.
#[derive(Debug, Clone, Serialize)]
pub struct SloPolicy {
    /// A step fails when its p95 round-trip exceeds `baseline_p95 * p95_multiplier`.
    pub p95_multiplier: f64,
    /// Maximum fraction of requests that may fail at the HTTP level.
    pub max_http_error_rate: f64,
    /// Maximum unsolved-order rate above the baseline's unsolved rate.
    pub max_excess_unsolved_rate: f64,
}

impl Default for SloPolicy {
    fn default() -> Self {
        Self { p95_multiplier: 1.2, max_http_error_rate: 0.001, max_excess_unsolved_rate: 0.001 }
    }
}

/// Unloaded reference measurements every step is compared against.
#[derive(Debug, Clone, Serialize)]
pub struct BaselineStats {
    pub requests: usize,
    pub unsolved_rate: f64,
    pub round_trip: TimingStats,
    pub solve_time: TimingStats,
}

/// Raw measurements from one ladder step.
#[derive(Debug)]
pub struct StepStats {
    pub target_rps: u64,
    pub requests_sent: usize,
    pub requests_succeeded: usize,
    pub orders_solved: usize,
    pub orders_unsolved: usize,
    pub duration_ms: u64,
    pub round_trip: TimingStats,
    pub solve_time: TimingStats,
}

/// One ladder step's measurements plus the SLO verdict.
#[derive(Debug, Serialize)]
pub struct StepOutcome {
    pub target_rps: u64,
    pub achieved_rps: f64,
    pub requests_sent: usize,
    pub requests_succeeded: usize,
    pub orders_solved: usize,
    pub orders_unsolved: usize,
    pub http_error_rate: f64,
    pub unsolved_rate: f64,
    pub round_trip: TimingStats,
    pub solve_time: TimingStats,
    pub passed: bool,
}

/// Applies the SLO policy to one step's measurements.
pub fn evaluate_step(policy: &SloPolicy, baseline: &BaselineStats, step: StepStats) -> StepOutcome {
    let http_error_rate = if step.requests_sent == 0 {
        1.0
    } else {
        step.requests_sent
            .saturating_sub(step.requests_succeeded) as f64 /
            step.requests_sent as f64
    };
    let orders_total = step.orders_solved + step.orders_unsolved;
    let unsolved_rate =
        if orders_total == 0 { 1.0 } else { step.orders_unsolved as f64 / orders_total as f64 };
    let excess_unsolved_rate = (unsolved_rate - baseline.unsolved_rate).max(0.0);
    // Round-trips are measured in whole milliseconds; a sub-millisecond baseline
    // truncates p95 to 0ms, making the limit 0 and failing every step regardless
    // of how fast the step actually was. Floor the baseline at 1ms before scaling.
    let p95_limit = baseline.round_trip.p95.max(1) as f64 * policy.p95_multiplier;
    let achieved_rps = if step.duration_ms == 0 {
        0.0
    } else {
        step.requests_succeeded as f64 * 1000.0 / step.duration_ms as f64
    };

    let passed = step.round_trip.p95 as f64 <= p95_limit &&
        http_error_rate <= policy.max_http_error_rate &&
        excess_unsolved_rate <= policy.max_excess_unsolved_rate;

    StepOutcome {
        target_rps: step.target_rps,
        achieved_rps,
        requests_sent: step.requests_sent,
        requests_succeeded: step.requests_succeeded,
        orders_solved: step.orders_solved,
        orders_unsolved: step.orders_unsolved,
        http_error_rate,
        unsolved_rate,
        round_trip: step.round_trip,
        solve_time: step.solve_time,
        passed,
    }
}

/// Full machine-readable output of one capacity run.
#[derive(Debug, Serialize)]
pub struct CapacityReport {
    pub timestamp_unix: u64,
    pub target_url: String,
    pub target_label: Option<String>,
    pub requests_file: Option<String>,
    pub requests_sha256: Option<String>,
    pub encoding: bool,
    pub timeout_ms: u64,
    pub step_duration_secs: u64,
    pub slo: SloPolicy,
    pub baseline: BaselineStats,
    pub steps: Vec<StepOutcome>,
    /// Highest target rate that passed the SLO; `None` if the first step failed.
    pub capacity_rps: Option<u64>,
}

/// Hex-encoded SHA-256 of `content`, recorded so runs over different request sets
/// are never compared as like-for-like.
pub fn sha256_hex(content: &str) -> String {
    use sha2::{Digest, Sha256};
    alloy::hex::encode(Sha256::digest(content.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(p95: u64) -> TimingStats {
        TimingStats {
            min: 1,
            max: p95,
            mean: p95 / 2,
            median: p95 / 2,
            p95,
            p99: p95,
            std_dev: 0.0,
        }
    }

    fn baseline() -> BaselineStats {
        BaselineStats {
            requests: 50,
            unsolved_rate: 0.02,
            round_trip: stats(100),
            solve_time: stats(80),
        }
    }

    fn step(p95: u64, sent: usize, succeeded: usize, unsolved: usize) -> StepStats {
        StepStats {
            target_rps: 10,
            requests_sent: sent,
            requests_succeeded: succeeded,
            orders_solved: succeeded - unsolved,
            orders_unsolved: unsolved,
            duration_ms: 60_000,
            round_trip: stats(p95),
            solve_time: stats(p95),
        }
    }

    // --- LadderSpec parsing ---
    #[test]
    fn ladder_parses() {
        let spec: LadderSpec = "5:5:200".parse().unwrap();
        assert_eq!(spec, LadderSpec { start: 5, step: 5, max: 200 });
    }
    #[test]
    fn ladder_rates_include_endpoints() {
        let spec: LadderSpec = "5:5:20".parse().unwrap();
        assert_eq!(spec.rates(), vec![5, 10, 15, 20]);
    }
    #[test]
    fn ladder_rates_non_divisible() {
        let spec: LadderSpec = "5:10:22".parse().unwrap();
        assert_eq!(spec.rates(), vec![5, 15]);
    }
    #[test]
    fn ladder_rejects_zero_start() {
        assert!("0:5:20".parse::<LadderSpec>().is_err());
    }
    #[test]
    fn ladder_rejects_zero_step() {
        assert!("5:0:20".parse::<LadderSpec>().is_err());
    }
    #[test]
    fn ladder_rejects_max_below_start() {
        assert!("50:5:20".parse::<LadderSpec>().is_err());
    }
    #[test]
    fn ladder_rejects_above_1000() {
        assert!("5:5:1001"
            .parse::<LadderSpec>()
            .is_err());
    }
    #[test]
    fn ladder_rejects_wrong_arity() {
        assert!("5:5".parse::<LadderSpec>().is_err());
        assert!("5:5:20:1"
            .parse::<LadderSpec>()
            .is_err());
        assert!("abc".parse::<LadderSpec>().is_err());
    }

    // --- evaluate_step ---
    #[test]
    fn step_passes_within_slo() {
        let outcome = evaluate_step(&SloPolicy::default(), &baseline(), step(110, 600, 600, 12));
        assert!(outcome.passed);
        assert!((outcome.achieved_rps - 10.0).abs() < 0.1);
    }
    #[test]
    fn step_fails_on_p95() {
        // 1.2 * 100 = 120; 121 breaches
        let outcome = evaluate_step(&SloPolicy::default(), &baseline(), step(121, 600, 600, 12));
        assert!(!outcome.passed);
    }
    #[test]
    fn step_fails_on_http_errors() {
        // 2/600 failed > 0.001
        let outcome = evaluate_step(&SloPolicy::default(), &baseline(), step(110, 600, 598, 12));
        assert!(!outcome.passed);
    }
    #[test]
    fn step_fails_on_excess_unsolved() {
        // unsolved 30/600 = 5% vs baseline 2% -> excess 3% > 0.1%
        let outcome = evaluate_step(&SloPolicy::default(), &baseline(), step(110, 600, 600, 30));
        assert!(!outcome.passed);
    }
    #[test]
    fn step_tolerates_baseline_unsolved() {
        // baseline already has 2% unsolved; matching it must pass
        let outcome = evaluate_step(&SloPolicy::default(), &baseline(), step(110, 600, 600, 12));
        assert!(outcome.passed);
    }
    #[test]
    fn step_passes_with_zero_baseline_p95() {
        // Baseline p95 truncated to 0ms must not floor the SLO limit to 0.
        let zero_baseline = BaselineStats {
            requests: 50,
            unsolved_rate: 0.02,
            round_trip: stats(0),
            solve_time: stats(0),
        };
        let outcome = evaluate_step(&SloPolicy::default(), &zero_baseline, step(1, 600, 600, 12));
        assert!(outcome.passed);
    }

    // --- sha256 ---
    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
