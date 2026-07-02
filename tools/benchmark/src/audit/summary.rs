//! Console summary: per-aggregator win rates and per-pair breakdowns.

use std::collections::BTreeSet;

use tracing::info;

use crate::audit::output::{ParticipantResult, TradeResult};

pub(crate) fn print_summary(results: &[TradeResult]) {
    let total = results.len();
    info!("{}", "=".repeat(80));
    info!("  FYND AUDIT RESULTS  ({total} trades)");
    info!("{}", "=".repeat(80));

    let baseline_name = results
        .iter()
        .find_map(|r| {
            r.participants
                .first()
                .map(|p| p.name.clone())
        })
        .unwrap_or_else(|| "fynd".to_string());

    let agg_names: Vec<String> = results
        .iter()
        .flat_map(|r| r.participants.iter())
        .filter(|p| p.name != baseline_name)
        .map(|p| p.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    for name in &agg_names {
        if print_aggregator_section(results, name) {
            print_per_pair_breakdown(results, name);
        }
    }

    info!("\n{}", "=".repeat(80));
}

/// Print the overall raw / gas-adjusted win rates for one aggregator. Returns `false` (and
/// prints a placeholder) when there are no comparable trades.
fn print_aggregator_section(results: &[TradeResult], name: &str) -> bool {
    info!("\n  vs {name}:");

    let raw_diffs = collect_diffs(results, name, |p| p.raw_diff_bps);
    if raw_diffs.is_empty() {
        info!("    No comparable trades.");
        return false;
    }

    let (rn, rw, ravg, rmed) = summarise(&raw_diffs);
    info!(
        "    Raw (no gas):       Fynd better {rw}/{rn} ({:.1}%)  avg {ravg:+.2} bps  median {rmed:+.2} bps",
        rw as f64 / rn as f64 * 100.0
    );

    print_gas_line(
        "    Gas-adj (reported):",
        "(no reported gas data)",
        &collect_diffs(results, name, |p| p.gas_adjusted_diff_bps_reported),
    );
    print_gas_line(
        "    Gas-adj (onchain): ",
        "(no on-chain gas data)",
        &collect_diffs(results, name, |p| p.gas_adjusted_diff_bps_onchain),
    );
    true
}

/// Print a gas-adjusted summary line, or `label empty_msg` when `diffs` is empty.
fn print_gas_line(label: &str, empty_msg: &str, diffs: &[f64]) {
    if diffs.is_empty() {
        info!("{label} {empty_msg}");
        return;
    }
    let (n, wins, avg, med) = summarise(diffs);
    info!(
        "{label} Fynd better {wins}/{n} ({:.1}%)  avg {avg:+.2} bps  median {med:+.2} bps",
        wins as f64 / n as f64 * 100.0
    );
}

fn print_per_pair_breakdown(results: &[TradeResult], name: &str) {
    info!("\n  Per-pair breakdown:");
    let mut pairs: Vec<&str> = results
        .iter()
        .map(|r| r.pair.as_str())
        .collect();
    pairs.sort_unstable();
    pairs.dedup();

    info!(
        "  {:<30}  {:>5}  {:>5}  {:>10}  {:>12}  {:>12}",
        "Pair", "n", "Win%", "Raw med", "Gas-rep med", "Gas-chain med"
    );
    info!("  {}", "-".repeat(80));

    for pair in &pairs {
        print_pair_row(results, name, pair);
    }
}

fn print_pair_row(results: &[TradeResult], name: &str, pair: &str) {
    let raw: Vec<f64> = pair_diffs(results, name, pair, |p| p.raw_diff_bps);
    if raw.is_empty() {
        return;
    }
    let mut gas_rep = pair_diffs(results, name, pair, |p| p.gas_adjusted_diff_bps_reported);
    let mut gas_chain = pair_diffs(results, name, pair, |p| p.gas_adjusted_diff_bps_onchain);

    let n = raw.len();
    let wins = raw.iter().filter(|&&d| d > 0.0).count();
    let mut raw_sorted = raw.clone();
    let raw_med = median(&mut raw_sorted).unwrap_or(0.0);

    let fmt_med = |v: Option<f64>| match v {
        Some(x) => format!("{:>+12.2}", x),
        None => "         n/a".to_string(),
    };

    info!(
        "  {:<30}  {:>5}  {:>4.1}%  {:>+10.2}  {}  {}",
        pair,
        n,
        wins as f64 / n as f64 * 100.0,
        raw_med,
        fmt_med(median(&mut gas_rep)),
        fmt_med(median(&mut gas_chain)),
    );
}

/// `(count, wins, mean, median)` over a non-empty slice of bps diffs.
fn summarise(diffs: &[f64]) -> (usize, usize, f64, f64) {
    let n = diffs.len();
    let wins = diffs
        .iter()
        .filter(|&&d| d > 0.0)
        .count();
    let avg = diffs.iter().sum::<f64>() / n as f64;
    let mut sorted = diffs.to_vec();
    sorted.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (n, wins, avg, sorted[n / 2])
}

/// Median of `vals` (sorts in place). `None` when empty.
fn median(vals: &mut [f64]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Some(vals[vals.len() / 2])
}

/// Collect a diff field across every row for `name`.
fn collect_diffs(
    results: &[TradeResult],
    name: &str,
    sel: impl Fn(&ParticipantResult) -> Option<f64>,
) -> Vec<f64> {
    results
        .iter()
        .flat_map(|r| r.participants.iter())
        .filter(|p| p.name.as_str() == name)
        .filter_map(sel)
        .collect()
}

/// Collect a diff field across every row for `name` within a single `pair`.
fn pair_diffs(
    results: &[TradeResult],
    name: &str,
    pair: &str,
    sel: impl Fn(&ParticipantResult) -> Option<f64>,
) -> Vec<f64> {
    results
        .iter()
        .filter(|r| r.pair.as_str() == pair)
        .flat_map(|r| r.participants.iter())
        .filter(|p| p.name.as_str() == name)
        .filter_map(sel)
        .collect()
}
