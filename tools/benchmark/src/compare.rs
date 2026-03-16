//! Compare subcommand.
//!
//! Sends identical quote requests to two solver instances in parallel and
//! reports per-request diffs in amount out (bps), gas estimate, route
//! selection, solve time, and optional net-of-gas output.

use std::time::{Duration, Instant};

use clap::Parser;
use fynd_client::{FyndClient, FyndClientBuilder, FyndError, Quote, QuoteStatus, RetryConfig};
use serde::Serialize;

use crate::requests::{generate_requests, load_requests_from_file, SwapRequest};

const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

/// Diff output quality between two running Fynd solvers.
///
/// Run two solvers on different ports (use git worktrees to avoid build
/// conflicts), then compare their quote responses head-to-head.
#[derive(Parser, Debug)]
#[command(
    about = "Compare output quality between two Fynd solvers",
    long_about = "Compare output quality between two Fynd solvers.\n\n\
        Sends identical quote requests to both in parallel and reports \
        amount-out diff (bps), gas estimate diff, solve time, route depth, \
        and optional net-of-gas comparison.\n\n\
        Requires two healthy solvers, typically on different ports."
)]
pub struct Args {
    /// Base URL of solver A (baseline)
    #[arg(long, default_value = "http://localhost:3000")]
    pub url_a: String,

    /// Base URL of solver B (candidate)
    #[arg(long, default_value = "http://localhost:3001")]
    pub url_b: String,

    /// Human-readable label for solver A in output
    #[arg(long, default_value = "main")]
    pub label_a: String,

    /// Human-readable label for solver B in output
    #[arg(long, default_value = "branch")]
    pub label_b: String,

    /// Number of quote requests to send to each solver
    #[arg(short = 'n', long, default_value_t = 500)]
    pub num_requests: usize,

    /// JSON file of request templates. Without this, requests are
    /// randomly generated from the built-in token-pair set.
    #[arg(long)]
    pub requests_file: Option<String>,

    /// Path to write full per-request results JSON
    #[arg(long, default_value = "comparison_results.json")]
    pub output: String,

    /// Per-request timeout in milliseconds
    #[arg(long, default_value_t = 15000)]
    pub timeout_ms: u64,

    /// RNG seed for deterministic request generation
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Ethereum RPC URL for gas price lookup (enables net-of-gas comparison)
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct Metrics {
    status: String,
    amount_in: String,
    amount_out: String,
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
    gas_estimate_diff_pct: Option<f64>,
    net_amount_out_diff_bps: Option<f64>,
    route_match: bool,
}

#[derive(Debug, Serialize)]
struct RequestResult {
    index: usize,
    label: String,
    token_in: String,
    token_out: String,
    metrics_a: Metrics,
    metrics_b: Metrics,
    comparison: Comparison,
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
    gas_price_wei: Option<u128>,
}

fn build_client(url: &str, timeout_ms: u64) -> anyhow::Result<FyndClient> {
    let client = FyndClientBuilder::new(url, "")
        .with_timeout(Duration::from_millis(timeout_ms))
        .with_retry(RetryConfig::new(
            1,
            Duration::from_millis(0),
            Duration::from_millis(0),
        ))
        .build_quote_only()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(client)
}

fn status_str(status: QuoteStatus) -> &'static str {
    match status {
        QuoteStatus::Success => "success",
        QuoteStatus::NoRouteFound => "no_route_found",
        QuoteStatus::InsufficientLiquidity => "insufficient_liquidity",
        QuoteStatus::Timeout => "timeout",
        QuoteStatus::NotReady => "not_ready",
    }
}

fn quote_metrics(quote: &Quote, round_trip_ms: u64) -> Metrics {
    let swaps = quote.route().map(|r| r.swaps()).unwrap_or_default();
    Metrics {
        status: status_str(quote.status()).to_string(),
        amount_in: quote.amount_in().to_string(),
        amount_out: quote.amount_out().to_string(),
        gas_estimate: quote.gas_estimate().to_string(),
        solve_time_ms: quote.solve_time_ms(),
        round_trip_ms,
        num_swaps: swaps.len(),
        route_protocols: swaps.iter().map(|s| s.protocol().to_string()).collect(),
    }
}

fn error_metrics(error: &FyndError, round_trip_ms: u64) -> Metrics {
    Metrics {
        status: format!("error: {error}"),
        amount_in: "0".to_string(),
        amount_out: "0".to_string(),
        gas_estimate: "0".to_string(),
        solve_time_ms: 0,
        round_trip_ms,
        num_swaps: 0,
        route_protocols: vec![],
    }
}

/// Estimate net-of-gas output using f64 arithmetic.
///
/// Returns `None` when the trade doesn't involve WETH (can't derive ETH/token price).
fn estimate_net_output(
    amount_out: u128,
    amount_in: u128,
    gas_estimate: u128,
    gas_price: u128,
    token_in: &str,
    token_out: &str,
) -> Option<f64> {
    let gas_cost_wei = gas_estimate as f64 * gas_price as f64;
    let out_f = amount_out as f64;

    if token_out.eq_ignore_ascii_case(WETH) {
        // amount_out is in WETH wei, gas_cost is in wei
        Some(out_f - gas_cost_wei)
    } else if token_in.eq_ignore_ascii_case(WETH) && amount_in > 0 {
        // Derive token_out per wei from the quote's own implied rate
        let rate = out_f / amount_in as f64;
        Some(out_f - gas_cost_wei * rate)
    } else {
        None
    }
}

/// Compute amount-out diff (bps), gas diff (%), net-of-gas diff, and route-match.
fn compare_metrics(
    a: &Metrics,
    b: &Metrics,
    gas_price: Option<u128>,
    token_in: &str,
    token_out: &str,
) -> Comparison {
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

    let gas_a: u128 = a.gas_estimate.parse().unwrap_or(0);
    let gas_b: u128 = b.gas_estimate.parse().unwrap_or(0);
    let gas_estimate_diff_pct = if gas_a > 0 && gas_b > 0 {
        Some((gas_b as f64 - gas_a as f64) * 100.0 / gas_a as f64)
    } else if gas_a == 0 && gas_b == 0 {
        Some(0.0)
    } else {
        None
    };

    let net_amount_out_diff_bps = gas_price.and_then(|gp| {
        let in_a: u128 = a.amount_in.parse().ok()?;
        let net_a = estimate_net_output(amt_a, in_a, gas_a, gp, token_in, token_out)?;
        let net_b = estimate_net_output(amt_b, in_a, gas_b, gp, token_in, token_out)?;
        if net_a > 0.0 {
            Some((net_b - net_a) * 10000.0 / net_a)
        } else {
            None
        }
    });

    let route_match = a.route_protocols == b.route_protocols;

    Comparison {
        status_match,
        amount_out_diff_bps,
        gas_estimate_diff_pct,
        net_amount_out_diff_bps,
        route_match,
    }
}

async fn send_quote(client: &FyndClient, req: &SwapRequest) -> (Result<Quote, FyndError>, u64) {
    let start = Instant::now();
    let result = client.quote(req.to_quote_params()).await;
    let round_trip_ms = start.elapsed().as_millis() as u64;
    (result, round_trip_ms)
}

async fn fetch_gas_price(rpc_url: &str) -> anyhow::Result<u128> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_gasPrice",
            "params": [],
            "id": 1,
        }))
        .send()
        .await?
        .json()
        .await?;
    let hex = resp["result"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing result in eth_gasPrice response"))?;
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    Ok(u128::from_str_radix(hex, 16)?)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Print an aggregated comparison summary with coverage, win rates, timing, and outliers.
fn print_summary(
    results: &[RequestResult],
    label_a: &str,
    label_b: &str,
    gas_price: Option<u128>,
) {
    let total = results.len();

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

    // -- Coverage --
    println!("\n  Coverage:");
    println!("    Both found route:          {both_success}");
    println!("    Only {label_a} found route:    {a_only}");
    println!("    Only {label_b} found route:    {b_only}");
    println!("    Neither found route:       {neither}");

    // -- Head-to-head (gross amount_out) --
    let diffs: Vec<f64> = results
        .iter()
        .filter_map(|r| r.comparison.amount_out_diff_bps)
        .collect();

    let b_wins = diffs.iter().filter(|&&d| d > 0.0).count();
    let a_wins = diffs.iter().filter(|&&d| d < 0.0).count();
    let ties = diffs.iter().filter(|&&d| d == 0.0).count();
    let contested = b_wins + a_wins + ties;

    if contested > 0 {
        println!("\n  Head-to-head ({contested} contested trades, amount out before gas):");
        println!(
            "    {label_b} wins:  {b_wins:>6}  ({:.1}%)",
            b_wins as f64 / contested as f64 * 100.0
        );
        println!(
            "    {label_a} wins:  {a_wins:>6}  ({:.1}%)",
            a_wins as f64 / contested as f64 * 100.0
        );
        println!(
            "    Ties:         {ties:>6}  ({:.1}%)",
            ties as f64 / contested as f64 * 100.0
        );

        let avg: f64 = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let min = diffs.iter().cloned().reduce(f64::min).unwrap_or(0.0);
        let max = diffs.iter().cloned().reduce(f64::max).unwrap_or(0.0);
        println!("    Avg diff:    {avg:+.2} bps  (min {min:+.2}, max {max:+.2})");
    }

    // -- Net-of-gas head-to-head --
    let net_diffs: Vec<f64> = results
        .iter()
        .filter_map(|r| r.comparison.net_amount_out_diff_bps)
        .collect();

    if !net_diffs.is_empty() {
        let net_b_wins = net_diffs.iter().filter(|&&d| d > 0.0).count();
        let net_a_wins = net_diffs.iter().filter(|&&d| d < 0.0).count();
        let net_ties = net_diffs.iter().filter(|&&d| d == 0.0).count();
        let net_total = net_b_wins + net_a_wins + net_ties;

        if net_total > 0 {
            let gp_gwei = gas_price.unwrap_or(0) as f64 / 1e9;
            println!(
                "\n  Head-to-head net of gas \
                 ({net_total} WETH-paired trades, gas price ~{gp_gwei:.1} gwei):"
            );
            println!(
                "    {label_b} wins:  {net_b_wins:>6}  ({:.1}%)",
                net_b_wins as f64 / net_total as f64 * 100.0
            );
            println!(
                "    {label_a} wins:  {net_a_wins:>6}  ({:.1}%)",
                net_a_wins as f64 / net_total as f64 * 100.0
            );
            println!(
                "    Ties:         {net_ties:>6}  ({:.1}%)",
                net_ties as f64 / net_total as f64 * 100.0
            );

            let net_avg: f64 = net_diffs.iter().sum::<f64>() / net_diffs.len() as f64;
            let net_min = net_diffs.iter().cloned().reduce(f64::min).unwrap_or(0.0);
            let net_max = net_diffs.iter().cloned().reduce(f64::max).unwrap_or(0.0);
            println!(
                "    Avg diff:    {net_avg:+.2} bps  \
                 (min {net_min:+.2}, max {net_max:+.2})"
            );
        }
    } else if gas_price.is_some() {
        println!(
            "\n  Net-of-gas: no WETH-paired trades found \
             (need WETH as token_in or token_out)"
        );
    }

    // -- Gas estimate --
    let gas_diffs: Vec<f64> = results
        .iter()
        .filter_map(|r| r.comparison.gas_estimate_diff_pct)
        .collect();

    if !gas_diffs.is_empty() {
        let gas_avg: f64 = gas_diffs.iter().sum::<f64>() / gas_diffs.len() as f64;
        let gas_b_lower = gas_diffs.iter().filter(|&&d| d < 0.0).count();
        let gas_a_lower = gas_diffs.iter().filter(|&&d| d > 0.0).count();
        println!("\n  Gas Estimate (negative = {label_b} cheaper):");
        println!("    {label_b} cheaper: {gas_b_lower}/{}", gas_diffs.len());
        println!("    {label_a} cheaper: {gas_a_lower}/{}", gas_diffs.len());
        println!("    Avg diff:     {gas_avg:+.2}%");
    }

    // -- Solve time --
    let mut times_a: Vec<u64> = results
        .iter()
        .filter(|r| r.metrics_a.status == "success")
        .map(|r| r.metrics_a.solve_time_ms)
        .collect();
    let mut times_b: Vec<u64> = results
        .iter()
        .filter(|r| r.metrics_b.status == "success")
        .map(|r| r.metrics_b.solve_time_ms)
        .collect();
    times_a.sort();
    times_b.sort();

    if !times_a.is_empty() && !times_b.is_empty() {
        let avg_a = times_a.iter().sum::<u64>() as f64 / times_a.len() as f64;
        let avg_b = times_b.iter().sum::<u64>() as f64 / times_b.len() as f64;

        println!("\n  Solve Time:");
        println!(
            "    {label_a}:  avg={avg_a:.0}ms  p50={}  p95={}  p99={}",
            percentile(&times_a, 50.0),
            percentile(&times_a, 95.0),
            percentile(&times_a, 99.0),
        );
        println!(
            "    {label_b}:  avg={avg_b:.0}ms  p50={}  p95={}  p99={}",
            percentile(&times_b, 50.0),
            percentile(&times_b, 95.0),
            percentile(&times_b, 99.0),
        );

        if avg_a > 0.0 {
            let pct = (avg_b - avg_a) / avg_a * 100.0;
            let word = if pct < 0.0 { "faster" } else { "slower" };
            println!(
                "    {label_b} is {:.1}% {word} on average",
                pct.abs()
            );
        }
    }

    // -- Route depth --
    let swaps_a: Vec<usize> = results
        .iter()
        .filter(|r| r.metrics_a.status == "success")
        .map(|r| r.metrics_a.num_swaps)
        .collect();
    let swaps_b: Vec<usize> = results
        .iter()
        .filter(|r| r.metrics_b.status == "success")
        .map(|r| r.metrics_b.num_swaps)
        .collect();

    if !swaps_a.is_empty() && !swaps_b.is_empty() {
        let avg_sa = swaps_a.iter().sum::<usize>() as f64 / swaps_a.len() as f64;
        let max_sa = swaps_a.iter().max().copied().unwrap_or(0);
        let avg_sb = swaps_b.iter().sum::<usize>() as f64 / swaps_b.len() as f64;
        let max_sb = swaps_b.iter().max().copied().unwrap_or(0);
        println!("\n  Route Depth:");
        println!("    {label_a} avg swaps: {avg_sa:.1}  (max {max_sa})");
        println!("    {label_b} avg swaps: {avg_sb:.1}  (max {max_sb})");

        let route_matches = results
            .iter()
            .filter(|r| r.comparison.route_match)
            .count();
        println!("    Identical routes:   {route_matches}/{total}");
    }

    // -- Significant outliers --
    let mut significant: Vec<&RequestResult> = results
        .iter()
        .filter(|r| {
            r.comparison
                .amount_out_diff_bps
                .is_some_and(|d| d.abs() > 1.0)
        })
        .collect();
    significant.sort_by(|a, b| {
        let da = a.comparison.amount_out_diff_bps.unwrap_or(0.0).abs();
        let db = b.comparison.amount_out_diff_bps.unwrap_or(0.0).abs();
        db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
    });
    if !significant.is_empty() {
        println!("\n  Significant differences (>1 bps):");
        for r in significant.iter().take(10) {
            let diff = r.comparison.amount_out_diff_bps.unwrap_or(0.0);
            let winner = if diff > 0.0 { label_b } else { label_a };
            println!(
                "    [{:>3}] {:<30} {diff:+.2} bps ({winner} better)",
                r.index, r.label,
            );
        }
    }

    // -- Route availability differences --
    if a_only > 0 || b_only > 0 {
        println!("\n  Route availability differences:");
        for r in results {
            let sa = &r.metrics_a.status;
            let sb = &r.metrics_b.status;
            if (sa == "success") != (sb == "success") {
                println!(
                    "    [{:>3}] {:<30} {label_a}={sa}, {label_b}={sb}",
                    r.index, r.label,
                );
            }
        }
    }

    println!("\n{}", "=".repeat(70));
}

/// Execute the comparison: health-check both solvers, send requests, print summary.
pub async fn run(args: Args) -> anyhow::Result<()> {
    fastrand::seed(args.seed);

    let client_a = build_client(&args.url_a, args.timeout_ms)?;
    let client_b = build_client(&args.url_b, args.timeout_ms)?;

    println!("Checking solver health...");

    let health_a = client_a
        .health()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !health_a.healthy() {
        return Err(anyhow::anyhow!("{}: solver is not healthy", args.label_a));
    }
    println!(
        "  {}: healthy (pools={}, last_update={}ms)",
        args.label_a,
        health_a.num_solver_pools(),
        health_a.last_update_ms(),
    );

    let health_b = client_b
        .health()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !health_b.healthy() {
        return Err(anyhow::anyhow!("{}: solver is not healthy", args.label_b));
    }
    println!(
        "  {}: healthy (pools={}, last_update={}ms)",
        args.label_b,
        health_b.num_solver_pools(),
        health_b.last_update_ms(),
    );

    let gas_price = match &args.rpc_url {
        Some(url) => {
            let gp = fetch_gas_price(url).await?;
            println!("  Gas price: {:.2} gwei", gp as f64 / 1e9);
            Some(gp)
        }
        None => None,
    };

    let requests: Vec<SwapRequest> = if let Some(ref path) = args.requests_file {
        load_requests_from_file(path, args.num_requests, args.timeout_ms)
            .map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        generate_requests(args.num_requests, args.timeout_ms)
    };

    println!(
        "\nSending {} requests to both solvers (parallel)...\n",
        args.num_requests
    );

    let mut results = Vec::with_capacity(args.num_requests);

    for (i, req) in requests.iter().enumerate() {
        let ((result_a, rt_a), (result_b, rt_b)) = tokio::join!(
            send_quote(&client_a, req),
            send_quote(&client_b, req),
        );

        let metrics_a = match &result_a {
            Ok(quote) => quote_metrics(quote, rt_a),
            Err(err) => error_metrics(err, rt_a),
        };

        let metrics_b = match &result_b {
            Ok(quote) => quote_metrics(quote, rt_b),
            Err(err) => error_metrics(err, rt_b),
        };

        let comparison = compare_metrics(
            &metrics_a,
            &metrics_b,
            gas_price,
            req.token_in_addr(),
            req.token_out_addr(),
        );

        let diff_str = comparison
            .amount_out_diff_bps
            .filter(|&d| d != 0.0)
            .map(|d| format!(" ({d:+.1} bps)"))
            .unwrap_or_default();

        let icon = if comparison.status_match && comparison.route_match {
            "="
        } else {
            "!"
        };

        println!(
            "  [{:>3}/{}] {icon} {:<30} A:{:<12} B:{:<12}{diff_str}",
            i + 1,
            args.num_requests,
            req.label,
            metrics_a.status,
            metrics_b.status,
        );

        results.push(RequestResult {
            index: i,
            label: req.label.clone(),
            token_in: req.token_in_addr().to_string(),
            token_out: req.token_out_addr().to_string(),
            metrics_a,
            metrics_b,
            comparison,
        });
    }

    print_summary(&results, &args.label_a, &args.label_b, gas_price);

    let output = Output {
        config: OutputConfig {
            url_a: args.url_a,
            url_b: args.url_b,
            label_a: args.label_a,
            label_b: args.label_b,
            num_requests: args.num_requests,
            timeout_ms: args.timeout_ms,
            seed: args.seed,
            gas_price_wei: gas_price,
        },
        results,
    };

    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(&args.output, &json)?;
    println!("\nFull results saved to: {}", args.output);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_metrics(
        status: &str,
        amount_out: &str,
        gas_estimate: &str,
        protocols: Vec<&str>,
    ) -> Metrics {
        Metrics {
            status: status.to_string(),
            amount_in: "1000000000000000000".to_string(),
            amount_out: amount_out.to_string(),
            gas_estimate: gas_estimate.to_string(),
            solve_time_ms: 0,
            round_trip_ms: 0,
            num_swaps: protocols.len(),
            route_protocols: protocols.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn compare_identical() {
        let a = make_metrics("success", "1000", "100", vec!["uniswap"]);
        let b = make_metrics("success", "1000", "100", vec!["uniswap"]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert!(c.status_match);
        assert!(c.route_match);
        assert_eq!(c.amount_out_diff_bps, Some(0.0));
        assert_eq!(c.gas_estimate_diff_pct, Some(0.0));
    }

    #[test]
    fn compare_b_better() {
        let a = make_metrics("success", "1000", "100", vec![]);
        let b = make_metrics("success", "1010", "100", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert_eq!(c.amount_out_diff_bps, Some(100.0));
    }

    #[test]
    fn compare_a_better() {
        let a = make_metrics("success", "1000", "100", vec![]);
        let b = make_metrics("success", "990", "100", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert_eq!(c.amount_out_diff_bps, Some(-100.0));
    }

    #[test]
    fn compare_a_zero() {
        let a = make_metrics("success", "0", "100", vec![]);
        let b = make_metrics("success", "1000", "100", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert_eq!(c.amount_out_diff_bps, None);
    }

    #[test]
    fn compare_b_zero() {
        let a = make_metrics("success", "1000", "100", vec![]);
        let b = make_metrics("success", "0", "100", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert_eq!(c.amount_out_diff_bps, None);
    }

    #[test]
    fn compare_both_zero() {
        let a = make_metrics("success", "0", "0", vec![]);
        let b = make_metrics("success", "0", "0", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert_eq!(c.amount_out_diff_bps, Some(0.0));
        assert_eq!(c.gas_estimate_diff_pct, Some(0.0));
    }

    #[test]
    fn compare_different_gas() {
        let a = make_metrics("success", "1000", "100", vec![]);
        let b = make_metrics("success", "1000", "120", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert_eq!(c.gas_estimate_diff_pct, Some(20.0));
    }

    #[test]
    fn compare_different_routes() {
        let a = make_metrics("success", "1000", "100", vec!["uniswap"]);
        let b = make_metrics("success", "1000", "100", vec!["curve"]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert!(!c.route_match);
    }

    #[test]
    fn compare_different_status() {
        let a = make_metrics("success", "1000", "100", vec![]);
        let b = make_metrics("error", "1000", "100", vec![]);
        let c = compare_metrics(&a, &b, None, "", "");
        assert!(!c.status_match);
    }

    #[test]
    fn status_str_all_variants() {
        assert_eq!(status_str(QuoteStatus::Success), "success");
        assert_eq!(status_str(QuoteStatus::NoRouteFound), "no_route_found");
        assert_eq!(
            status_str(QuoteStatus::InsufficientLiquidity),
            "insufficient_liquidity"
        );
        assert_eq!(status_str(QuoteStatus::Timeout), "timeout");
        assert_eq!(status_str(QuoteStatus::NotReady), "not_ready");
    }

    #[test]
    fn net_output_weth_out() {
        // token_out is WETH: gas cost subtracted directly
        let net = estimate_net_output(1_000_000, 500, 100, 10, "0xtoken", WETH);
        // gas_cost_wei = 100 * 10 = 1000
        assert_eq!(net, Some(1_000_000.0 - 1000.0));
    }

    #[test]
    fn net_output_weth_in() {
        // token_in is WETH: gas cost converted via implied rate
        let net = estimate_net_output(2_000_000, 1_000_000, 100, 10, WETH, "0xtoken");
        // rate = 2_000_000 / 1_000_000 = 2.0
        // gas_cost_wei = 100 * 10 = 1000
        // gas_cost_token_out = 1000 * 2.0 = 2000
        assert_eq!(net, Some(2_000_000.0 - 2000.0));
    }

    #[test]
    fn net_output_no_weth() {
        let net = estimate_net_output(1_000_000, 500_000, 100, 10, "0xA", "0xB");
        assert_eq!(net, None);
    }

    #[test]
    fn net_output_zero_amount_in() {
        let net = estimate_net_output(1_000_000, 0, 100, 10, WETH, "0xtoken");
        assert_eq!(net, None);
    }

    #[test]
    fn compare_with_gas_price_weth_out() {
        let mut a = make_metrics("success", "1000000", "100", vec![]);
        a.amount_in = "500000".to_string();
        let mut b = make_metrics("success", "1000000", "200", vec![]);
        b.amount_in = "500000".to_string();
        let c = compare_metrics(&a, &b, Some(10), "0xtoken", WETH);
        // Gross: equal (0 bps)
        assert_eq!(c.amount_out_diff_bps, Some(0.0));
        // Net: B uses more gas, so net should be negative
        assert!(c.net_amount_out_diff_bps.unwrap() < 0.0);
    }

    #[test]
    fn compare_without_gas_price_no_net() {
        let a = make_metrics("success", "1000", "100", vec![]);
        let b = make_metrics("success", "1010", "100", vec![]);
        let c = compare_metrics(&a, &b, None, WETH, "0xtoken");
        assert_eq!(c.net_amount_out_diff_bps, None);
    }

    #[test]
    fn percentile_empty() {
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn percentile_single() {
        assert_eq!(percentile(&[42], 50.0), 42);
        assert_eq!(percentile(&[42], 99.0), 42);
    }

    #[test]
    fn percentile_sorted() {
        let data: Vec<u64> = (1..=100).collect();
        // p50 of 1..=100: index = round(99 * 0.5) = 50 (0-based) = value 51
        assert_eq!(percentile(&data, 50.0), 51);
        assert_eq!(percentile(&data, 0.0), 1);
        assert_eq!(percentile(&data, 100.0), 100);
    }
}
