//! `capacity` subcommand: steps an RPS ladder against a solver until the latency
//! SLO breaks, then reports the highest sustainable rate.
//!
//! An unloaded sequential baseline is measured first; each ladder step then fires
//! rate-based traffic for a fixed duration (after a discarded warm-up window) and
//! is judged by [`evaluate_step`]. The report JSON is always the last thing
//! printed to stdout, after a `=== CAPACITY REPORT JSON ===` marker, so
//! in-cluster Jobs can retrieve it from the pod logs (marker to EOF, minus the
//! marker line, is exactly the JSON).

use std::{sync::Arc, time::Instant};

use clap::Parser;
use fynd_client::{FyndClient, FyndClientBuilder};
use tracing::info;

use crate::{
    benchmark::check_solver_health,
    capacity_report::{
        evaluate_step, sha256_hex, BaselineStats, CapacityReport, LadderSpec, SloPolicy,
        StepOutcome, StepStats,
    },
    config::{ParallelizationMode, TimingStats},
    requests::{load_embedded_trades, load_request_templates, SwapRequest},
    runner::RunnerResults,
};

/// Marker line printed before the report JSON on stdout.
pub const REPORT_MARKER: &str = "=== CAPACITY REPORT JSON ===";

/// Measure the highest request rate a solver sustains within the latency SLO.
#[derive(Parser, Debug)]
#[command(
    about = "Step an RPS ladder against a solver until p95 breaches the SLO",
    long_about = "Step an RPS ladder against a solver until p95 breaches the SLO.\n\n\
        Establishes an unloaded baseline first; capacity is the last passing rate.\n\
        Always build with --release for accurate measurements."
)]
pub struct Args {
    /// Base URL of the solver to measure
    #[arg(long, env = "SOLVER_URL", default_value = "http://localhost:3000")]
    pub solver_url: String,

    /// JSON file of request templates; defaults to the embedded 50-trade sample
    #[arg(long, env = "REQUESTS_FILE")]
    pub requests_file: Option<String>,

    /// RPS ladder as start:step:max (e.g. 5:5:200)
    #[arg(long, env = "LADDER", default_value = "5:5:200")]
    pub ladder: String,

    /// Seconds each ladder step is measured (after warm-up)
    #[arg(long, env = "STEP_DURATION_SECS", default_value = "60")]
    pub step_duration_secs: u64,

    /// Seconds of discarded warm-up traffic before each step
    #[arg(long, env = "WARMUP_SECS", default_value = "5")]
    pub warmup_secs: u64,

    /// Number of sequential requests for the unloaded baseline
    #[arg(long, env = "BASELINE_REQUESTS", default_value = "50")]
    pub baseline_requests: usize,

    /// p95 degradation multiplier that fails a step
    #[arg(long, env = "SLO_MULTIPLIER", default_value = "1.2")]
    pub slo_multiplier: f64,

    /// Per-request quote timeout in milliseconds
    #[arg(long, env = "TIMEOUT_MS", default_value = "5000")]
    pub timeout_ms: u64,

    /// Attach standard encoding options to every request
    #[arg(long, env = "ENCODING")]
    pub encoding: bool,

    /// Free-form label recorded in the report (e.g. image tag + pod config)
    #[arg(long, env = "TARGET_LABEL")]
    pub target_label: Option<String>,

    /// Also write the JSON report to this file
    #[arg(long, env = "OUTPUT_FILE")]
    pub output_file: Option<String>,

    /// RNG seed for request sampling (fixed for cross-run comparability)
    #[arg(long, env = "SEED", default_value = "42")]
    pub seed: u64,
}

/// Ladder execution parameters (separated from CLI parsing for testability).
pub(crate) struct LadderParams {
    pub ladder: LadderSpec,
    pub step_duration_secs: u64,
    pub warmup_secs: u64,
    pub baseline_requests: usize,
    pub policy: SloPolicy,
    pub encoding: bool,
}

/// Execute the `capacity` subcommand: health-check, run the ladder, print the report.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let ladder: LadderSpec = args
        .ladder
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    fastrand::seed(args.seed);
    let output_file = args.output_file.clone();

    let client = Arc::new(
        FyndClientBuilder::new(&args.solver_url)
            .build_quote_only()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );
    check_solver_health(&client).await?;

    let (templates, requests_sha256) = load_templates(&args)?;
    info!("Loaded {} request template(s)", templates.len());

    let params = LadderParams {
        ladder,
        step_duration_secs: args.step_duration_secs,
        warmup_secs: args.warmup_secs,
        baseline_requests: args.baseline_requests,
        policy: SloPolicy { p95_multiplier: args.slo_multiplier, ..SloPolicy::default() },
        encoding: args.encoding,
    };
    let (baseline, steps, capacity_rps) =
        run_ladder(Arc::clone(&client), &templates, &params).await?;

    let report = CapacityReport {
        timestamp_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        target_url: args.solver_url,
        target_label: args.target_label,
        requests_file: args.requests_file,
        requests_sha256,
        encoding: args.encoding,
        timeout_ms: args.timeout_ms,
        step_duration_secs: args.step_duration_secs,
        slo: params.policy,
        baseline,
        steps,
        capacity_rps,
    };

    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = &output_file {
        std::fs::write(path, &json)
            .map_err(|e| anyhow::anyhow!("cannot write report to '{path}': {e}"))?;
    }
    // Human-readable summary first, marker + JSON last: everything from the
    // marker to EOF (minus the marker line) must be exactly the JSON so
    // in-cluster Jobs can capture it from pod logs with a single sed.
    match report.capacity_rps {
        Some(rps) => println!("Capacity: {rps} rps at SLO\n"),
        None => println!("Capacity: SLO breached at the first ladder step\n"),
    }
    println!("{REPORT_MARKER}");
    println!("{json}");
    Ok(())
}

fn load_templates(args: &Args) -> anyhow::Result<(Vec<SwapRequest>, Option<String>)> {
    if let Some(path) = &args.requests_file {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read requests file '{path}': {e}"))?;
        let templates =
            load_request_templates(path, args.timeout_ms).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((templates, Some(sha256_hex(&content))))
    } else {
        let templates =
            load_embedded_trades(50, args.timeout_ms).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((templates, None))
    }
}

/// Runs baseline + ladder. Returns (baseline, step outcomes, capacity).
pub(crate) async fn run_ladder(
    client: Arc<FyndClient>,
    templates: &[SwapRequest],
    params: &LadderParams,
) -> anyhow::Result<(BaselineStats, Vec<StepOutcome>, Option<u64>)> {
    info!("Measuring unloaded baseline ({} sequential requests)", params.baseline_requests);
    let baseline_results = ParallelizationMode::Sequential
        .run(Arc::clone(&client), templates, params.baseline_requests, params.encoding)
        .await;
    let baseline = baseline_stats(&baseline_results, params.baseline_requests)?;
    info!(
        "Baseline: p95 round-trip {}ms, unsolved rate {:.1}%",
        baseline.round_trip.p95,
        baseline.unsolved_rate * 100.0
    );

    let mut steps = Vec::new();
    let mut capacity_rps = None;
    for target_rps in params.ladder.rates() {
        let interval_ms = (1000 / target_rps).max(1);
        let mode = ParallelizationMode::RateBased { interval_ms };

        let warmup_requests = (target_rps * params.warmup_secs) as usize;
        if warmup_requests > 0 {
            info!("Warm-up: {}s at {} rps (discarded)", params.warmup_secs, target_rps);
            mode.run(Arc::clone(&client), templates, warmup_requests, params.encoding)
                .await;
        }

        let step_requests = (target_rps * params.step_duration_secs) as usize;
        info!(
            "Step: {}s at {} rps ({} requests)",
            params.step_duration_secs, target_rps, step_requests
        );
        let step_start = Instant::now();
        let results = mode
            .run(Arc::clone(&client), templates, step_requests, params.encoding)
            .await;
        let duration_ms = step_start.elapsed().as_millis() as u64;

        let zeroed_failed = TimingStats {
            min: 0,
            max: 0,
            mean: 0,
            median: 0,
            p95: u64::MAX,
            p99: u64::MAX,
            std_dev: 0.0,
        };
        let round_trip = TimingStats::from_measurements(&results.round_trip_times)
            .unwrap_or_else(|| zeroed_failed.clone());
        let solve_time =
            TimingStats::from_measurements(&results.solve_times).unwrap_or(zeroed_failed);
        let outcome = evaluate_step(
            &params.policy,
            &baseline,
            StepStats {
                target_rps,
                requests_sent: step_requests,
                requests_succeeded: results.successful_requests,
                orders_solved: results.orders_solved,
                orders_unsolved: results.orders_unsolved,
                duration_ms,
                round_trip,
                solve_time,
            },
        );
        info!(
            "Step {} rps: p95 {}ms, http errors {:.2}%, unsolved {:.1}% -> {}",
            target_rps,
            outcome.round_trip.p95,
            outcome.http_error_rate * 100.0,
            outcome.unsolved_rate * 100.0,
            if outcome.passed { "PASS" } else { "FAIL" }
        );
        let passed = outcome.passed;
        steps.push(outcome);
        if passed {
            capacity_rps = Some(target_rps);
        } else {
            break;
        }
    }
    Ok((baseline, steps, capacity_rps))
}

fn baseline_stats(results: &RunnerResults, requested: usize) -> anyhow::Result<BaselineStats> {
    let round_trip =
        TimingStats::from_measurements(&results.round_trip_times).ok_or_else(|| {
            anyhow::anyhow!("baseline produced no successful requests; is the solver healthy?")
        })?;
    let solve_time = TimingStats::from_measurements(&results.solve_times)
        .ok_or_else(|| anyhow::anyhow!("baseline produced no solve times"))?;
    let orders_total = results.orders_solved + results.orders_unsolved;
    let unsolved_rate =
        if orders_total == 0 { 1.0 } else { results.orders_unsolved as f64 / orders_total as f64 };
    Ok(BaselineStats { requests: requested, unsolved_rate, round_trip, solve_time })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fynd_client::FyndClientBuilder;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    fn minimal_quote_json() -> serde_json::Value {
        serde_json::json!({
            "orders": [{
                "order_id": "cap-1",
                "status": "success",
                "amount_in": "1000000",
                "amount_out": "990000",
                "gas_estimate": "50000",
                "amount_out_net_gas": "940000",
                "price_impact_bps": 10,
                "block": {"number": 1, "hash": "0xabc", "timestamp": 1700000000}
            }],
            "total_gas_estimate": "50000",
            "solve_time_ms": 5
        })
    }

    #[tokio::test]
    async fn ladder_completes_against_fast_mock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/quote"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(minimal_quote_json())
                    // Round-trips are measured in whole milliseconds; an undelayed
                    // local mock truncates the baseline p95 to 0ms, making the SLO
                    // limit (baseline * multiplier) zero and every step fail.
                    .set_delay(std::time::Duration::from_millis(20)),
            )
            .mount(&server)
            .await;
        let client = Arc::new(
            FyndClientBuilder::new(server.uri())
                .build_quote_only()
                .unwrap(),
        );
        let templates = vec![crate::requests::default_request(5000)];
        let params = LadderParams {
            ladder: "2:2:4".parse().unwrap(),
            step_duration_secs: 1,
            warmup_secs: 0,
            baseline_requests: 5,
            // SLO math is unit-tested in capacity_report; this test covers orchestration only.
            // A fast mock's ~1ms baseline puts the default 1.2x multiplier inside OS jitter,
            // so use thresholds that cannot flake.
            policy: SloPolicy { p95_multiplier: 100.0, ..SloPolicy::default() },
            encoding: false,
        };

        let (baseline, steps, capacity) = run_ladder(client, &templates, &params)
            .await
            .unwrap();

        assert_eq!(baseline.requests, 5);
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.passed));
        assert_eq!(capacity, Some(4));
    }
}
