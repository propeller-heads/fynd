//! Report generator: CSV export + markdown summary.

use std::{collections::BTreeMap, path::Path};

use num_bigint::BigUint;
use num_traits::ToPrimitive;

use crate::types::{AuditRow, RowStatus};

/// Derive ETH-denominated error from a row's `error_gas` and `gas_price_wei`.
/// Returns `None` if the row didn't produce a measurable error (no quote /
/// reverted simulation).
pub(crate) fn error_eth(row: &AuditRow) -> Option<f64> {
    let gas = row.error_gas?;
    Some(gas as f64 * biguint_to_f64(&row.gas_price_wei) / 1e18)
}

fn biguint_to_f64(b: &BigUint) -> f64 {
    b.to_f64().unwrap_or(0.0)
}

/// Top-level summary statistics over the successful rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub attempted: usize,
    pub succeeded: usize,
    pub no_quote: usize,
    pub no_encoding: usize,
    pub simulation_reverted: usize,
    pub mean_error_eth: f64,
    pub median_error_eth: f64,
    pub p95_abs_error_eth: f64,
    pub mean_abs_pct: f64,
    pub over_charged: usize,
    pub under_charged: usize,
    pub sum_signed_eth: f64,
    pub sum_abs_eth: f64,
}

pub fn summarize(rows: &[AuditRow]) -> Summary {
    let mut summary = Summary { attempted: rows.len(), ..Default::default() };
    for r in rows {
        match r.status {
            RowStatus::Success => summary.succeeded += 1,
            RowStatus::NoQuote => summary.no_quote += 1,
            RowStatus::NoEncoding => summary.no_encoding += 1,
            RowStatus::SimulationReverted => summary.simulation_reverted += 1,
        }
    }

    let successes: Vec<&AuditRow> = rows
        .iter()
        .filter(|r| r.status == RowStatus::Success)
        .collect();
    let errors: Vec<f64> = successes
        .iter()
        .filter_map(|r| error_eth(r))
        .collect();
    if errors.is_empty() {
        return summary;
    }

    let abs_errors: Vec<f64> = errors.iter().map(|x| x.abs()).collect();
    summary.mean_error_eth = mean(&errors);
    summary.median_error_eth = median(&errors);
    summary.p95_abs_error_eth = percentile(&abs_errors, 95.0);
    summary.sum_abs_eth = abs_errors.iter().sum();
    summary.sum_signed_eth = errors.iter().sum();
    summary.over_charged = errors
        .iter()
        .filter(|x| **x > 0.0)
        .count();
    summary.under_charged = errors
        .iter()
        .filter(|x| **x < 0.0)
        .count();

    let pct: Vec<f64> = successes
        .iter()
        .filter_map(|r| {
            let actual = r.actual_gas? as f64;
            let err_eth = error_eth(r)?;
            let actual_cost_eth = actual * biguint_to_f64(&r.gas_price_wei) / 1e18;
            (actual_cost_eth != 0.0).then(|| (err_eth / actual_cost_eth).abs() * 100.0)
        })
        .collect();
    summary.mean_abs_pct = if pct.is_empty() { 0.0 } else { mean(&pct) };

    summary
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n.is_multiple_of(2) {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    } else {
        v[n / 2]
    }
}

fn percentile(xs: &[f64], p: f64) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    let rank = (p / 100.0) * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        v[lo]
    } else {
        let frac = rank - lo as f64;
        v[lo] * (1.0 - frac) + v[hi] * frac
    }
}

/// Write `results.csv` with one row per trade. `AuditRow` derives `Serialize`
/// so the column layout is whatever serde produces — keep field order on the
/// struct stable if downstream tooling reads by position.
pub fn write_csv(path: &Path, rows: &[AuditRow]) -> anyhow::Result<()> {
    let mut wtr = csv::Writer::from_path(path)?;
    for r in rows {
        wtr.serialize(r)?;
    }
    wtr.flush()?;
    Ok(())
}

/// Aggregate stats over a slice of *successful* rows. Empty input yields zeros.
#[derive(Default)]
struct GroupStats {
    count: usize,
    under: usize,
    over: usize,
    mean_pct: f64,
    mean_error_eth: f64,
    sum_error_eth: f64,
}

fn group_stats(rows: &[&AuditRow]) -> GroupStats {
    let mut errors_eth: Vec<f64> = Vec::new();
    let mut pcts: Vec<f64> = Vec::new();
    let mut under = 0usize;
    let mut over = 0usize;
    for r in rows {
        let Some(err_eth) = error_eth(r) else { continue };
        let Some(actual) = r.actual_gas else { continue };
        errors_eth.push(err_eth);
        if err_eth > 0.0 {
            over += 1;
        } else if err_eth < 0.0 {
            under += 1;
        }
        let actual_cost = actual as f64 * biguint_to_f64(&r.gas_price_wei) / 1e18;
        if actual_cost > 0.0 {
            pcts.push((err_eth / actual_cost).abs() * 100.0);
        }
    }
    let count = errors_eth.len();
    if count == 0 {
        return GroupStats::default();
    }
    GroupStats {
        count,
        under,
        over,
        mean_pct: if pcts.is_empty() { 0.0 } else { mean(&pcts) },
        mean_error_eth: mean(&errors_eth),
        sum_error_eth: errors_eth.iter().sum(),
    }
}

fn write_group_table_header(out: &mut String) -> std::fmt::Result {
    use std::fmt::Write;
    writeln!(
        out,
        "\n| group | n | under | over | mean \\|err\\|/cost | mean err (ETH) | sum err (ETH) |"
    )?;
    writeln!(out, "|---|---|---|---|---|---|---|")
}

fn write_group_row(out: &mut String, label: &str, s: &GroupStats) -> std::fmt::Result {
    use std::fmt::Write;
    writeln!(
        out,
        "| {} | {} | {} | {} | {:.2}% | {:+.6} | {:+.6} |",
        label, s.count, s.under, s.over, s.mean_pct, s.mean_error_eth, s.sum_error_eth
    )
}

/// Group `rows` by the label produced by `group_fn`, then write one stats row
/// per non-empty group. Rows where `group_fn` returns `None` are skipped.
fn write_breakdown<F>(out: &mut String, rows: &[&AuditRow], group_fn: F) -> std::fmt::Result
where
    F: Fn(&AuditRow) -> Option<String>,
{
    let mut by_group: BTreeMap<String, Vec<&AuditRow>> = BTreeMap::new();
    for r in rows {
        if let Some(label) = group_fn(r) {
            by_group
                .entry(label)
                .or_default()
                .push(*r);
        }
    }
    write_group_table_header(out)?;
    for (label, group) in &by_group {
        let stats = group_stats(group);
        write_group_row(out, label, &stats)?;
    }
    Ok(())
}

/// Categorise a successful row by route shape.
fn route_shape_label(r: &AuditRow) -> Option<&'static str> {
    match r.num_swaps {
        Some(1) => Some("single"),
        Some(n) if n > 1 => Some("sequential"),
        _ => None,
    }
}

/// Write `report.md` with the aggregate table and worst-10 trades.
pub fn write_markdown(
    path: &Path,
    gas_price_wei: &BigUint,
    eth_price_usd: Option<f64>,
    summary: &Summary,
    rows: &[AuditRow],
) -> anyhow::Result<()> {
    use std::fmt::Write;

    let mut out = String::new();
    writeln!(&mut out, "# Fynd Gas-Estimation Audit")?;
    writeln!(&mut out)?;
    writeln!(
        &mut out,
        "**Gas price used:** {} wei ({:.2} gwei)",
        gas_price_wei,
        biguint_to_f64(gas_price_wei) / 1e9
    )?;
    if let Some(p) = eth_price_usd {
        writeln!(&mut out, "**ETH price (context only, not used in math):** ${p:.2}")?;
    }
    writeln!(&mut out)?;
    writeln!(
        &mut out,
        "**Trades:** {} attempted, {} succeeded, {} no-quote, {} no-encoding, {} simulation-reverted",
        summary.attempted,
        summary.succeeded,
        summary.no_quote,
        summary.no_encoding,
        summary.simulation_reverted
    )?;
    if summary.attempted - summary.succeeded > 20 {
        writeln!(
            &mut out,
            "\n> FINDING: more than 20 trades excluded from aggregates — review error reasons in `results.csv`.\n"
        )?;
    }

    writeln!(&mut out, "\n## Aggregate error")?;
    writeln!(&mut out, "\n| metric | value |")?;
    writeln!(&mut out, "|---|---|")?;
    writeln!(&mut out, "| Mean signed error (ETH) | {:+.6} |", summary.mean_error_eth)?;
    writeln!(&mut out, "| Median signed error (ETH) | {:+.6} |", summary.median_error_eth)?;
    writeln!(&mut out, "| P95 absolute error (ETH) | {:.6} |", summary.p95_abs_error_eth)?;
    writeln!(&mut out, "| Mean \\|error\\| / actual cost | {:.2}% |", summary.mean_abs_pct)?;
    writeln!(
        &mut out,
        "| Trades over-charged | {} / {} |",
        summary.over_charged, summary.succeeded
    )?;
    writeln!(
        &mut out,
        "| Trades under-charged | {} / {} |",
        summary.under_charged, summary.succeeded
    )?;
    writeln!(&mut out, "| Sum of signed error (ETH) | {:+.6} |", summary.sum_signed_eth)?;
    writeln!(&mut out, "| Sum of absolute error (ETH) | {:.6} |", summary.sum_abs_eth)?;

    writeln!(&mut out, "\n## Worst 10 trades by absolute error")?;
    writeln!(
        &mut out,
        "\n| token_in | token_out | amount_in | gas_estimate | actual_gas | error_eth |"
    )?;
    writeln!(&mut out, "|---|---|---|---|---|---|")?;
    let mut successes: Vec<&AuditRow> = rows
        .iter()
        .filter(|r| r.status == RowStatus::Success)
        .collect();
    let abs_err = |r: &&AuditRow| error_eth(r).unwrap_or(0.0).abs();
    successes.sort_by(|a, b| abs_err(b).total_cmp(&abs_err(a)));
    for r in successes.iter().take(10) {
        writeln!(
            &mut out,
            "| {} | {} | {} | {} | {} | {:+.6} |",
            r.token_in,
            r.token_out,
            r.amount_in,
            r.gas_estimate
                .as_ref()
                .map(BigUint::to_string)
                .unwrap_or_default(),
            r.actual_gas
                .map(|g| g.to_string())
                .unwrap_or_default(),
            error_eth(r).unwrap_or(0.0)
        )?;
    }

    writeln!(&mut out, "\n## By route shape")?;
    write_breakdown(&mut out, &successes, |r| route_shape_label(r).map(String::from))?;

    writeln!(&mut out, "\n## By protocol — single-hop only")?;
    write_breakdown(&mut out, &successes, |r| {
        if r.num_swaps == Some(1) {
            r.protocols.clone()
        } else {
            None
        }
    })?;

    let any_seq = successes
        .iter()
        .any(|r| matches!(r.num_swaps, Some(n) if n > 1));
    if any_seq {
        writeln!(&mut out, "\n## By protocol sequence — sequential routes")?;
        write_breakdown(&mut out, &successes, |r| {
            if matches!(r.num_swaps, Some(n) if n > 1) {
                r.protocols.clone()
            } else {
                None
            }
        })?;
    }

    writeln!(&mut out, "\n## Interpretation")?;
    writeln!(
        &mut out,
        "\n- **Per-trade accuracy** — the distribution shows how reliably Fynd sizes individual trades. Governs whether the \"best-by-`amount_out_net_gas`\" route selection picks the right route."
    )?;
    writeln!(
        &mut out,
        "- **Portfolio bias** — the signed total shows whether Fynd is systematically optimistic or pessimistic across a realistic mix of trades."
    )?;

    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `error_gas` of 50_000 at 20 gwei → +0.001 ETH; -100_000 → -0.002 ETH.
    fn row(status: RowStatus, error_gas: Option<i128>, actual: Option<u64>) -> AuditRow {
        AuditRow {
            token_in: "0xa".into(),
            token_out: "0xb".into(),
            amount_in: "1".into(),
            gas_estimate: Some(BigUint::from(200_000u64)),
            actual_gas: actual,
            gas_price_wei: BigUint::from(20_000_000_000u64),
            error_gas,
            status,
            error_reason: None,
            num_swaps: None,
            protocols: None,
        }
    }

    #[test]
    fn summarize_counts_statuses() {
        let rows = vec![
            row(RowStatus::Success, Some(50_000), Some(150_000)),
            row(RowStatus::Success, Some(-100_000), Some(220_000)),
            row(RowStatus::NoQuote, None, None),
            row(RowStatus::SimulationReverted, None, None),
        ];
        let s = summarize(&rows);
        assert_eq!(s.attempted, 4);
        assert_eq!(s.succeeded, 2);
        assert_eq!(s.no_quote, 1);
        assert_eq!(s.simulation_reverted, 1);
        assert_eq!(s.over_charged, 1);
        assert_eq!(s.under_charged, 1);
    }

    #[test]
    fn summarize_empty_returns_zeros() {
        let s = summarize(&[row(RowStatus::NoQuote, None, None)]);
        assert_eq!(s.succeeded, 0);
        assert_eq!(s.mean_error_eth, 0.0);
        assert_eq!(s.sum_abs_eth, 0.0);
    }

    #[test]
    fn write_csv_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.csv");
        let rows = vec![row(RowStatus::Success, Some(50_000), Some(150_000))];
        write_csv(&path, &rows).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("success"));
        assert!(text.contains("0xa"));
        assert!(text.contains("0xb"));
    }

    #[test]
    fn write_markdown_flags_high_exclusion_rate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        let mut rows = Vec::new();
        for _ in 0..80 {
            rows.push(row(RowStatus::Success, Some(0), Some(200_000)));
        }
        for _ in 0..21 {
            rows.push(row(RowStatus::NoQuote, None, None));
        }
        let summary = summarize(&rows);
        write_markdown(&path, &BigUint::from(20_000_000_000u64), None, &summary, &rows).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("more than 20 trades excluded"));
    }

    #[test]
    fn write_markdown_no_finding_when_exclusion_low() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        let mut rows = Vec::new();
        for _ in 0..95 {
            rows.push(row(RowStatus::Success, Some(0), Some(200_000)));
        }
        for _ in 0..5 {
            rows.push(row(RowStatus::NoQuote, None, None));
        }
        let summary = summarize(&rows);
        write_markdown(&path, &BigUint::from(20_000_000_000u64), None, &summary, &rows).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("more than 20 trades excluded"));
    }
}
