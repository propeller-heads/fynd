//! `quality` subcommand: offline, deterministic routing-algorithm comparison.
//!
//! Replays a captured market snapshot in-process and runs each algorithm over the same trades, then
//! reports output-quality differences. Unlike `compare`, this needs no running solver and no
//! network: results are fully reproducible, which is what makes algorithm iteration practical.

use std::{path::PathBuf, str::FromStr};

use anyhow::{Context, Result};
use fynd_core::{
    offline::{self, OfflineSolution},
    AlgorithmConfig, Order, OrderSide,
};
use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_core::Bytes;

/// Mainnet WETH, the default gas token used to price gas during derived-data computation.
const DEFAULT_GAS_TOKEN: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

/// Compare routing algorithms offline on a captured market snapshot.
#[derive(clap::Parser, Debug)]
pub struct Args {
    /// Path to a market snapshot JSON (produced by the `capture_snapshot` example).
    #[arg(short, long, default_value = "market_snapshot.json")]
    pub snapshot: PathBuf,

    /// Path to the trades dataset JSON (e.g. aggregator_trades_10k.json).
    #[arg(short, long, default_value = "aggregator_trades_10k.json")]
    pub requests_file: PathBuf,

    /// Number of trades to sample (0 = all).
    #[arg(short, long, default_value_t = 0)]
    pub num_requests: usize,

    /// Comma-separated algorithm names to compare.
    #[arg(short, long, default_value = "most_liquid,bellman_ford")]
    pub algorithms: String,

    /// Baseline algorithm to measure improvements against.
    #[arg(short, long, default_value = "most_liquid")]
    pub baseline: String,

    /// Maximum hops to search.
    #[arg(long, default_value_t = 4)]
    pub max_hops: usize,

    /// Per-solve timeout in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub timeout_ms: u64,

    /// Gas token address used during derived-data computation.
    #[arg(long, default_value = DEFAULT_GAS_TOKEN)]
    pub gas_token: String,

    /// Random sampling seed (for reproducible subsets).
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Optional path to write full per-trade results as JSON.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Deserialize)]
struct TradeEntry {
    orders: Vec<TradeOrder>,
}

#[derive(Deserialize)]
struct TradeOrder {
    token_in: String,
    token_out: String,
    amount: String,
    side: String,
}

#[derive(Serialize)]
struct PerTradeResult {
    index: usize,
    token_in: String,
    token_out: String,
    amount: String,
    /// Per-algorithm net_amount_out (decimal string); null if no route.
    nets: Vec<Option<String>>,
}

#[derive(Serialize)]
struct AlgorithmReport {
    algorithm: String,
    /// Trades for which a route was found.
    coverage: usize,
    /// Sum of net_amount_out over the set where every algorithm succeeded.
    total_net_common: String,
    /// Trades strictly better than the baseline (on the common set).
    wins_vs_baseline: usize,
    /// Trades strictly worse than the baseline (on the common set).
    losses_vs_baseline: usize,
    /// Mean improvement over the baseline in basis points (common set, baseline net > 0).
    mean_improvement_bps: f64,
    /// Median improvement over the baseline in basis points.
    median_improvement_bps: f64,
    /// Median per-solve latency over solved orders, milliseconds.
    p50_solve_ms: f64,
    /// 95th-percentile per-solve latency over solved orders, milliseconds.
    p95_solve_ms: f64,
    /// Maximum per-solve latency over solved orders, milliseconds.
    max_solve_ms: f64,
}

pub async fn run(args: Args) -> Result<()> {
    let algorithms: Vec<String> = args
        .algorithms
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    anyhow::ensure!(!algorithms.is_empty(), "no algorithms specified");
    anyhow::ensure!(
        algorithms.contains(&args.baseline),
        "baseline '{}' must be among --algorithms",
        args.baseline
    );

    println!("Loading snapshot: {}", args.snapshot.display());
    let snapshot = offline::load_snapshot(&args.snapshot)
        .with_context(|| format!("loading snapshot {}", args.snapshot.display()))?;

    let orders = load_orders(&args)?;
    println!("Loaded {} trades", orders.len());

    let gas_token = Bytes::from_str(&args.gas_token).context("parsing --gas-token")?;
    println!("Computing derived data (gas token {})...", args.gas_token);
    let (market, derived) = offline::prepare(snapshot, gas_token, args.max_hops, 0.01)
        .await
        .context("preparing derived data")?;

    let config = AlgorithmConfig::new(
        1,
        args.max_hops,
        std::time::Duration::from_millis(args.timeout_ms),
        None,
    )?;

    let mut all_results: Vec<Vec<Option<OfflineSolution>>> = Vec::new();
    for name in &algorithms {
        println!("Running {name} over {} trades...", orders.len());
        let results = offline::run_algorithm(&market, &derived, name, config.clone(), &orders)
            .await
            .with_context(|| format!("running algorithm {name}"))?;
        all_results.push(results);
    }

    let reports = aggregate(&algorithms, &args.baseline, &all_results);
    print_summary(&orders, &algorithms, &reports);

    if let Some(path) = &args.output {
        write_output(path, &orders, &algorithms, &all_results, &reports)?;
        println!("\nWrote full results to {}", path.display());
    }

    Ok(())
}

fn load_orders(args: &Args) -> Result<Vec<Order>> {
    let bytes = std::fs::read(&args.requests_file)
        .with_context(|| format!("reading trades file {}", args.requests_file.display()))?;
    let entries: Vec<TradeEntry> = serde_json::from_slice(&bytes).context("parsing trades JSON")?;

    let mut orders = Vec::new();
    for entry in entries {
        let Some(trade) = entry.orders.into_iter().next() else {
            continue;
        };
        if trade.side != "sell" {
            continue;
        }
        let (Ok(token_in), Ok(token_out), Ok(amount)) = (
            Bytes::from_str(&trade.token_in),
            Bytes::from_str(&trade.token_out),
            BigUint::from_str(&trade.amount),
        ) else {
            continue;
        };
        if token_in == token_out || amount.is_zero() {
            continue;
        }
        let sender = Bytes::from([0x01u8; 20].as_slice());
        orders.push(Order::new(token_in, token_out, amount, OrderSide::Sell, sender));
    }

    if args.num_requests > 0 && args.num_requests < orders.len() {
        let mut rng = fastrand::Rng::with_seed(args.seed);
        rng.shuffle(&mut orders);
        orders.truncate(args.num_requests);
    }
    Ok(orders)
}

fn aggregate(
    algorithms: &[String],
    baseline: &str,
    all_results: &[Vec<Option<OfflineSolution>>],
) -> Vec<AlgorithmReport> {
    let baseline_idx = algorithms
        .iter()
        .position(|a| a == baseline)
        .expect("baseline present");
    let num_trades = all_results
        .first()
        .map(|r| r.len())
        .unwrap_or(0);

    // Common set: trades where every algorithm found a route.
    let common: Vec<usize> = (0..num_trades)
        .filter(|&i| {
            all_results
                .iter()
                .all(|r| r[i].is_some())
        })
        .collect();

    let mut reports = Vec::new();
    for (algo_idx, name) in algorithms.iter().enumerate() {
        let results = &all_results[algo_idx];
        let coverage = results
            .iter()
            .filter(|r| r.is_some())
            .count();

        let mut total_net_common = BigInt::zero();
        let mut wins = 0usize;
        let mut losses = 0usize;
        let mut improvements_bps: Vec<f64> = Vec::new();

        for &i in &common {
            let algo_net = &results[i]
                .as_ref()
                .unwrap()
                .net_amount_out;
            let base_net = &all_results[baseline_idx][i]
                .as_ref()
                .unwrap()
                .net_amount_out;
            total_net_common += algo_net;

            if algo_net > base_net {
                wins += 1;
            } else if algo_net < base_net {
                losses += 1;
            }
            if base_net > &BigInt::zero() {
                let diff = algo_net - base_net;
                if let (Some(d), Some(b)) = (diff.to_f64(), base_net.to_f64()) {
                    improvements_bps.push(d / b * 10_000.0);
                }
            }
        }

        let mean = if improvements_bps.is_empty() {
            0.0
        } else {
            improvements_bps.iter().sum::<f64>() / improvements_bps.len() as f64
        };
        let median = median(&mut improvements_bps.clone());

        // Per-solve latency over solved orders (microseconds → ms).
        let mut latencies: Vec<u64> = results
            .iter()
            .filter_map(|r| r.as_ref().map(|s| s.solve_micros))
            .collect();
        latencies.sort_unstable();
        let pct = |p: f64| -> f64 {
            if latencies.is_empty() {
                return 0.0;
            }
            let idx = ((latencies.len() as f64 * p).ceil() as usize)
                .saturating_sub(1)
                .min(latencies.len() - 1);
            latencies[idx] as f64 / 1000.0
        };

        reports.push(AlgorithmReport {
            algorithm: name.clone(),
            coverage,
            total_net_common: total_net_common.to_string(),
            wins_vs_baseline: wins,
            losses_vs_baseline: losses,
            mean_improvement_bps: mean,
            median_improvement_bps: median,
            p50_solve_ms: pct(0.50),
            p95_solve_ms: pct(0.95),
            max_solve_ms: latencies
                .last()
                .map(|m| *m as f64 / 1000.0)
                .unwrap_or(0.0),
        });
    }
    reports
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn print_summary(orders: &[Order], algorithms: &[String], reports: &[AlgorithmReport]) {
    let num_trades = orders.len();
    let common = reports
        .iter()
        .map(|r| r.coverage)
        .min()
        .unwrap_or(0);

    println!("\n========== Routing Quality (offline) ==========");
    println!("Trades: {num_trades}");
    println!("Algorithms: {}", algorithms.join(", "));
    println!(
        "\n{:<16} {:>9} {:>14} {:>8} {:>8} {:>11} {:>11} {:>9} {:>9} {:>9}",
        "algorithm",
        "coverage",
        "total_net",
        "wins",
        "losses",
        "mean_bps",
        "median_bps",
        "p50_ms",
        "p95_ms",
        "max_ms",
    );
    for r in reports {
        println!(
            "{:<16} {:>9} {:>14} {:>8} {:>8} {:>11.2} {:>11.2} {:>9.2} {:>9.2} {:>9.2}",
            r.algorithm,
            r.coverage,
            truncate_net(&r.total_net_common),
            r.wins_vs_baseline,
            r.losses_vs_baseline,
            r.mean_improvement_bps,
            r.median_improvement_bps,
            r.p50_solve_ms,
            r.p95_solve_ms,
            r.max_solve_ms,
        );
    }
    println!("\n(wins/losses/bps measured against the baseline on the common-success set; min coverage ~{common})");
    println!("total_net is the summed net_amount_out over the common set (shown truncated).");
}

/// Net sums get huge (wei); show the magnitude compactly.
fn truncate_net(s: &str) -> String {
    let neg = s.starts_with('-');
    let digits = s.trim_start_matches('-');
    let shown = if digits.len() > 12 {
        format!("{}e{}", &digits[..4], digits.len() - 4)
    } else {
        digits.to_string()
    };
    if neg {
        format!("-{shown}")
    } else {
        shown
    }
}

fn write_output(
    path: &PathBuf,
    orders: &[Order],
    algorithms: &[String],
    all_results: &[Vec<Option<OfflineSolution>>],
    reports: &[AlgorithmReport],
) -> Result<()> {
    let mut per_trade = Vec::with_capacity(orders.len());
    for (i, order) in orders.iter().enumerate() {
        let nets = all_results
            .iter()
            .map(|r| {
                r[i].as_ref()
                    .map(|s| s.net_amount_out.to_string())
            })
            .collect();
        per_trade.push(PerTradeResult {
            index: i,
            token_in: order.token_in().to_string(),
            token_out: order.token_out().to_string(),
            amount: order.amount().to_string(),
            nets,
        });
    }

    #[derive(Serialize)]
    struct Output<'a> {
        algorithms: &'a [String],
        reports: &'a [AlgorithmReport],
        trades: Vec<PerTradeResult>,
    }

    let output = Output { algorithms, reports, trades: per_trade };
    let json = serde_json::to_vec_pretty(&output)?;
    std::fs::write(path, json)?;
    Ok(())
}
