mod requests;

use std::time::Instant;

use clap::Parser;
use requests::{generate_requests, load_requests_from_file, SwapRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Compare solver output quality between two running instances.
///
/// Start solver A (e.g. main) on port 3000 and solver B (e.g. branch) on port 3001,
/// then run this tool to send identical requests to both and compare results.
#[derive(Parser, Debug)]
#[command(name = "compare")]
struct Cli {
    /// Solver A URL
    #[arg(long, default_value = "http://localhost:3000")]
    url_a: String,

    /// Solver B URL
    #[arg(long, default_value = "http://localhost:3001")]
    url_b: String,

    /// Label for solver A
    #[arg(long, default_value = "main")]
    label_a: String,

    /// Label for solver B
    #[arg(long, default_value = "branch")]
    label_b: String,

    /// Number of requests to send
    #[arg(short = 'n', long, default_value_t = 100)]
    num_requests: usize,

    /// Path to requests JSON file (benchmark format)
    #[arg(long)]
    requests_file: Option<String>,

    /// Output file for full results JSON
    #[arg(long, default_value = "comparison_results.json")]
    output: String,

    /// Request timeout in milliseconds
    #[arg(long, default_value_t = 15000)]
    timeout_ms: u64,

    /// Random seed for reproducibility
    #[arg(long, default_value_t = 42)]
    seed: u64,
}

#[derive(Debug, Serialize)]
struct Metrics {
    status: String,
    amount_out: String,
    amount_out_net_gas: String,
    gas_estimate: String,
    solve_time_ms: u64,
    round_trip_ms: u64,
    num_swaps: usize,
    route_protocols: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Comparison {
    status_match: bool,
    amount_out_diff_bps: Option<f64>,
    net_gas_diff_bps: Option<f64>,
    route_match: bool,
}

#[derive(Debug, Serialize)]
struct RequestResult {
    index: usize,
    label: String,
    request: Value,
    metrics_a: Metrics,
    metrics_b: Metrics,
    comparison: Comparison,
    response_a: Value,
    response_b: Value,
}

#[derive(Debug, Serialize)]
struct Output {
    config: OutputConfig,
    results: Vec<RequestResult>,
}

#[derive(Debug, Serialize)]
struct OutputConfig {
    url_a: String,
    url_b: String,
    label_a: String,
    label_b: String,
    num_requests: usize,
    timeout_ms: u64,
    seed: u64,
}

#[derive(Deserialize)]
struct HealthResponse {
    healthy: bool,
    num_solver_pools: Option<usize>,
    last_update_ms: Option<u64>,
}

fn extract_metrics(response: &Value, round_trip_ms: u64) -> Metrics {
    let order = response
        .get("orders")
        .and_then(|o| o.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);

    let swaps = order
        .get("route")
        .and_then(|r| r.get("swaps"))
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    Metrics {
        status: order
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("error")
            .to_string(),
        amount_out: order
            .get("amount_out")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string(),
        amount_out_net_gas: order
            .get("amount_out_net_gas")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string(),
        gas_estimate: order
            .get("gas_estimate")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string(),
        solve_time_ms: response
            .get("solve_time_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        round_trip_ms,
        num_swaps: swaps.len(),
        route_protocols: swaps
            .iter()
            .filter_map(|s| {
                s.get("protocol")
                    .and_then(|p| p.as_str())
            })
            .map(String::from)
            .collect(),
    }
}

fn error_metrics(error: &str, round_trip_ms: u64) -> Metrics {
    Metrics {
        status: format!("error: {error}"),
        amount_out: "0".to_string(),
        amount_out_net_gas: "0".to_string(),
        gas_estimate: "0".to_string(),
        solve_time_ms: 0,
        round_trip_ms,
        num_swaps: 0,
        route_protocols: vec![],
    }
}

fn compare_metrics(a: &Metrics, b: &Metrics) -> Comparison {
    let status_match = a.status == b.status;

    let amt_a: u128 = a.amount_out.parse().unwrap_or(0);
    let amt_b: u128 = b.amount_out.parse().unwrap_or(0);
    let amount_out_diff_bps = if amt_a > 0 && amt_b > 0 {
        Some((amt_b as f64 - amt_a as f64) * 10000.0 / amt_a as f64)
    } else if amt_a == 0 && amt_b == 0 {
        Some(0.0)
    } else {
        None
    };

    let net_a: u128 = a
        .amount_out_net_gas
        .parse()
        .unwrap_or(0);
    let net_b: u128 = b
        .amount_out_net_gas
        .parse()
        .unwrap_or(0);
    let net_gas_diff_bps = if net_a > 0 && net_b > 0 {
        Some((net_b as f64 - net_a as f64) * 10000.0 / net_a as f64)
    } else if net_a == 0 && net_b == 0 {
        Some(0.0)
    } else {
        None
    };

    let route_match = a.route_protocols == b.route_protocols;

    Comparison { status_match, amount_out_diff_bps, net_gas_diff_bps, route_match }
}

async fn check_health(
    client: &reqwest::Client,
    url: &str,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp: HealthResponse = client
        .get(format!("{url}/v1/health"))
        .send()
        .await?
        .json()
        .await?;

    if !resp.healthy {
        return Err(format!("{label}: solver is not healthy").into());
    }

    println!(
        "  {label}: healthy (pools={}, last_update={}ms)",
        resp.num_solver_pools.unwrap_or(0),
        resp.last_update_ms.unwrap_or(0),
    );
    Ok(())
}

async fn send_quote(client: &reqwest::Client, url: &str, body: &Value) -> (Value, u64) {
    let start = Instant::now();
    let result = client
        .post(format!("{url}/v1/quote"))
        .json(body)
        .send()
        .await;
    let round_trip_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(json) => (json, round_trip_ms),
            Err(e) => (serde_json::json!({"_error": format!("parse error: {e}")}), round_trip_ms),
        },
        Err(e) => (serde_json::json!({"_error": format!("{e}")}), round_trip_ms),
    }
}

fn print_summary(results: &[RequestResult], label_a: &str, label_b: &str) {
    let total = results.len();
    let status_matches = results
        .iter()
        .filter(|r| r.comparison.status_match)
        .count();
    let route_matches = results
        .iter()
        .filter(|r| r.comparison.route_match)
        .count();

    let diffs: Vec<f64> = results
        .iter()
        .filter_map(|r| r.comparison.amount_out_diff_bps)
        .collect();

    let net_diffs: Vec<f64> = results
        .iter()
        .filter_map(|r| r.comparison.net_gas_diff_bps)
        .collect();

    let b_better = diffs
        .iter()
        .filter(|&&d| d > 0.0)
        .count();
    let a_better = diffs
        .iter()
        .filter(|&&d| d < 0.0)
        .count();
    let equal = diffs
        .iter()
        .filter(|&&d| d == 0.0)
        .count();

    let both_success = results
        .iter()
        .filter(|r| r.metrics_a.status == "success" && r.metrics_b.status == "success")
        .count();
    let a_only = results
        .iter()
        .filter(|r| r.metrics_a.status == "success" && r.metrics_b.status != "success")
        .count();
    let b_only = results
        .iter()
        .filter(|r| r.metrics_a.status != "success" && r.metrics_b.status == "success")
        .count();
    let neither = total - both_success - a_only - b_only;

    println!("\n{}", "=".repeat(70));
    println!("  SOLVER COMPARISON: {label_a} vs {label_b}");
    println!("  {total} requests");
    println!("{}", "=".repeat(70));

    println!("\n  Status:");
    println!("    Both found route:      {both_success}");
    println!("    Only {label_a} found route: {a_only}");
    println!("    Only {label_b} found route: {b_only}");
    println!("    Neither found route:   {neither}");
    println!("    Status match rate:     {status_matches}/{total}");

    println!("\n  Route:");
    println!("    Identical routes:      {route_matches}/{total}");

    if !diffs.is_empty() {
        println!("\n  Amount Out (positive = {label_b} better):");
        println!("    {label_b} better: {b_better}/{}", diffs.len());
        println!("    {label_a} better: {a_better}/{}", diffs.len());
        println!("    Equal:       {equal}/{}", diffs.len());
        let avg: f64 = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let min = diffs
            .iter()
            .cloned()
            .reduce(f64::min)
            .unwrap_or(0.0);
        let max = diffs
            .iter()
            .cloned()
            .reduce(f64::max)
            .unwrap_or(0.0);
        println!("    Avg diff:    {avg:+.2} bps");
        println!("    Min diff:    {min:+.2} bps");
        println!("    Max diff:    {max:+.2} bps");
    }

    if !net_diffs.is_empty() {
        let avg: f64 = net_diffs.iter().sum::<f64>() / net_diffs.len() as f64;
        let min = net_diffs
            .iter()
            .cloned()
            .reduce(f64::min)
            .unwrap_or(0.0);
        let max = net_diffs
            .iter()
            .cloned()
            .reduce(f64::max)
            .unwrap_or(0.0);
        println!("\n  Amount Out Net Gas (positive = {label_b} better):");
        println!("    Avg diff:    {avg:+.2} bps");
        println!("    Min diff:    {min:+.2} bps");
        println!("    Max diff:    {max:+.2} bps");
    }

    // Significant outliers
    let mut significant: Vec<&RequestResult> = results
        .iter()
        .filter(|r| {
            r.comparison
                .amount_out_diff_bps
                .is_some_and(|d| d.abs() > 1.0)
        })
        .collect();
    significant.sort_by(|a, b| {
        let da = a
            .comparison
            .amount_out_diff_bps
            .unwrap_or(0.0)
            .abs();
        let db = b
            .comparison
            .amount_out_diff_bps
            .unwrap_or(0.0)
            .abs();
        db.partial_cmp(&da)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if !significant.is_empty() {
        println!("\n  Significant differences (>1 bps):");
        for r in significant.iter().take(10) {
            let diff = r
                .comparison
                .amount_out_diff_bps
                .unwrap_or(0.0);
            let winner = if diff > 0.0 { label_b } else { label_a };
            println!("    [{:>3}] {:<30} {diff:+.2} bps ({winner} better)", r.index, r.label,);
        }
    }

    // Route availability differences
    if a_only > 0 || b_only > 0 {
        println!("\n  Route availability differences:");
        for r in results {
            let sa = &r.metrics_a.status;
            let sb = &r.metrics_b.status;
            if (sa == "success") != (sb == "success") {
                println!("    [{:>3}] {:<30} {label_a}={sa}, {label_b}={sb}", r.index, r.label,);
            }
        }
    }

    println!("\n{}", "=".repeat(70));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    fastrand::seed(cli.seed);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(cli.timeout_ms))
        .build()?;

    println!("Checking solver health...");
    check_health(&client, &cli.url_a, &cli.label_a).await?;
    check_health(&client, &cli.url_b, &cli.label_b).await?;

    let requests: Vec<SwapRequest> = if let Some(ref path) = cli.requests_file {
        load_requests_from_file(path, cli.num_requests, cli.timeout_ms)?
    } else {
        generate_requests(cli.num_requests, cli.timeout_ms)
    };

    println!("\nSending {} requests to both solvers...\n", cli.num_requests);

    let mut results = Vec::with_capacity(cli.num_requests);

    for (i, req) in requests.iter().enumerate() {
        let (resp_a, rt_a) = send_quote(&client, &cli.url_a, &req.body).await;
        let (resp_b, rt_b) = send_quote(&client, &cli.url_b, &req.body).await;

        let is_error_a = resp_a.get("_error").is_some();
        let is_error_b = resp_b.get("_error").is_some();

        let metrics_a = if is_error_a {
            let msg = resp_a["_error"]
                .as_str()
                .unwrap_or("unknown");
            error_metrics(msg, rt_a)
        } else {
            extract_metrics(&resp_a, rt_a)
        };

        let metrics_b = if is_error_b {
            let msg = resp_b["_error"]
                .as_str()
                .unwrap_or("unknown");
            error_metrics(msg, rt_b)
        } else {
            extract_metrics(&resp_b, rt_b)
        };

        let comparison = compare_metrics(&metrics_a, &metrics_b);

        let diff_str = comparison
            .amount_out_diff_bps
            .filter(|&d| d != 0.0)
            .map(|d| format!(" ({d:+.1} bps)"))
            .unwrap_or_default();

        let icon = if comparison.status_match && comparison.route_match { "=" } else { "!" };

        println!(
            "  [{:>3}/{}] {icon} {:<30} A:{:<12} B:{:<12}{diff_str}",
            i + 1,
            cli.num_requests,
            req.label,
            metrics_a.status,
            metrics_b.status,
        );

        results.push(RequestResult {
            index: i,
            label: req.label.clone(),
            request: req.body.clone(),
            metrics_a,
            metrics_b,
            comparison,
            response_a: resp_a,
            response_b: resp_b,
        });
    }

    print_summary(&results, &cli.label_a, &cli.label_b);

    let output = Output {
        config: OutputConfig {
            url_a: cli.url_a,
            url_b: cli.url_b,
            label_a: cli.label_a,
            label_b: cli.label_b,
            num_requests: cli.num_requests,
            timeout_ms: cli.timeout_ms,
            seed: cli.seed,
        },
        results,
    };

    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(&cli.output, &json)?;
    println!("\nFull results saved to: {}", cli.output);

    Ok(())
}
