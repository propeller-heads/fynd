//! Report generator: CSV export + markdown summary.

use std::path::Path;

use num_bigint::BigUint;

use crate::types::{AuditRow, RowStatus};

/// Top-level summary statistics over the successful rows.
#[derive(Debug, Clone, PartialEq)]
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
    let attempted = rows.len();
    let mut succeeded = 0;
    let mut no_quote = 0;
    let mut no_encoding = 0;
    let mut simulation_reverted = 0;
    for r in rows {
        match r.status {
            RowStatus::Success => succeeded += 1,
            RowStatus::NoQuote => no_quote += 1,
            RowStatus::NoEncoding => no_encoding += 1,
            RowStatus::SimulationReverted => simulation_reverted += 1,
        }
    }

    let successful_errors: Vec<f64> = rows
        .iter()
        .filter(|r| r.status == RowStatus::Success)
        .filter_map(|r| r.error_eth)
        .collect();

    if successful_errors.is_empty() {
        return Summary {
            attempted,
            succeeded,
            no_quote,
            no_encoding,
            simulation_reverted,
            mean_error_eth: 0.0,
            median_error_eth: 0.0,
            p95_abs_error_eth: 0.0,
            mean_abs_pct: 0.0,
            over_charged: 0,
            under_charged: 0,
            sum_signed_eth: 0.0,
            sum_abs_eth: 0.0,
        };
    }

    let mean_error_eth = mean(&successful_errors);
    let median_error_eth = median(&successful_errors);

    let abs_errors: Vec<f64> = successful_errors
        .iter()
        .map(|x| x.abs())
        .collect();
    let p95_abs_error_eth = percentile(&abs_errors, 95.0);
    let sum_abs_eth = abs_errors.iter().sum();
    let sum_signed_eth = successful_errors.iter().sum();

    let over_charged = successful_errors
        .iter()
        .filter(|x| **x > 0.0)
        .count();
    let under_charged = successful_errors
        .iter()
        .filter(|x| **x < 0.0)
        .count();

    let pct: Vec<f64> = rows
        .iter()
        .filter(|r| r.status == RowStatus::Success)
        .filter_map(|r| {
            let actual = r.actual_gas? as f64;
            let err_eth = r.error_eth?;
            let price_wei = r
                .gas_price_wei
                .to_string()
                .parse::<f64>()
                .ok()?;
            let actual_cost_eth = actual * price_wei / 1e18;
            if actual_cost_eth == 0.0 {
                None
            } else {
                Some((err_eth / actual_cost_eth).abs() * 100.0)
            }
        })
        .collect();
    let mean_abs_pct = if pct.is_empty() { 0.0 } else { mean(&pct) };

    Summary {
        attempted,
        succeeded,
        no_quote,
        no_encoding,
        simulation_reverted,
        mean_error_eth,
        median_error_eth,
        p95_abs_error_eth,
        mean_abs_pct,
        over_charged,
        under_charged,
        sum_signed_eth,
        sum_abs_eth,
    }
}

fn mean(xs: &[f64]) -> f64 {
    let sum: f64 = xs.iter().sum();
    sum / xs.len() as f64
}

fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let n = v.len();
    #[allow(clippy::manual_is_multiple_of)]
    if n % 2 == 0 {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    } else {
        v[n / 2]
    }
}

fn percentile(xs: &[f64], p: f64) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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

/// Write `results.csv` with one row per trade.
pub fn write_csv(path: &Path, rows: &[AuditRow]) -> anyhow::Result<()> {
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record([
        "token_in",
        "token_out",
        "amount_in",
        "gas_estimate",
        "actual_gas",
        "gas_price_wei",
        "error_gas",
        "error_wei",
        "error_eth",
        "status",
        "error_reason",
        "num_swaps",
        "protocols",
    ])?;
    for r in rows {
        wtr.write_record([
            r.token_in.clone(),
            r.token_out.clone(),
            r.amount_in.clone(),
            r.gas_estimate
                .as_ref()
                .map(BigUint::to_string)
                .unwrap_or_default(),
            r.actual_gas
                .map(|g| g.to_string())
                .unwrap_or_default(),
            r.gas_price_wei.to_string(),
            r.error_gas
                .map(|g| g.to_string())
                .unwrap_or_default(),
            r.error_wei
                .map(|w| w.to_string())
                .unwrap_or_default(),
            r.error_eth
                .map(|e| format!("{e:.12}"))
                .unwrap_or_default(),
            match r.status {
                RowStatus::Success => "success",
                RowStatus::NoQuote => "no_quote",
                RowStatus::NoEncoding => "no_encoding",
                RowStatus::SimulationReverted => "simulation_reverted",
            }
            .to_string(),
            r.error_reason
                .clone()
                .unwrap_or_default(),
            r.num_swaps
                .map(|n| n.to_string())
                .unwrap_or_default(),
            r.protocols.clone().unwrap_or_default(),
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

/// Aggregate stats over a slice of *successful* rows. Empty input yields zeros.
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
        let Some(err_eth) = r.error_eth else { continue };
        let Some(actual) = r.actual_gas else { continue };
        errors_eth.push(err_eth);
        if err_eth > 0.0 {
            over += 1;
        } else if err_eth < 0.0 {
            under += 1;
        }
        let price = r
            .gas_price_wei
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0);
        let actual_cost = actual as f64 * price / 1e18;
        if actual_cost > 0.0 {
            pcts.push((err_eth / actual_cost).abs() * 100.0);
        }
    }
    let count = errors_eth.len();
    if count == 0 {
        return GroupStats {
            count: 0,
            under: 0,
            over: 0,
            mean_pct: 0.0,
            mean_error_eth: 0.0,
            sum_error_eth: 0.0,
        };
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

/// Categorise a successful row by route shape.
fn route_shape_label(r: &AuditRow) -> &'static str {
    match r.num_swaps {
        Some(1) => "single",
        Some(n) if n > 1 => "sequential",
        _ => "unknown",
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

fn write_route_shape_breakdown(out: &mut String, successes: &[&AuditRow]) -> std::fmt::Result {
    use std::fmt::Write;
    writeln!(out, "\n## By route shape")?;
    write_group_table_header(out)?;
    for label in ["single", "sequential"] {
        let group: Vec<&AuditRow> = successes
            .iter()
            .copied()
            .filter(|r| route_shape_label(r) == label)
            .collect();
        let stats = group_stats(&group);
        write_group_row(out, label, &stats)?;
    }
    Ok(())
}

fn write_protocol_breakdown(out: &mut String, successes: &[&AuditRow]) -> std::fmt::Result {
    use std::fmt::Write;

    // Single-hop: clean per-protocol attribution (1 row → 1 protocol).
    writeln!(out, "\n## By protocol — single-hop only")?;
    let single: Vec<&AuditRow> = successes
        .iter()
        .copied()
        .filter(|r| r.num_swaps == Some(1))
        .collect();
    let mut by_protocol: std::collections::BTreeMap<String, Vec<&AuditRow>> =
        std::collections::BTreeMap::new();
    for r in &single {
        if let Some(p) = r.protocols.as_deref() {
            by_protocol
                .entry(p.to_string())
                .or_default()
                .push(*r);
        }
    }
    write_group_table_header(out)?;
    for (proto, rows) in &by_protocol {
        let stats = group_stats(rows);
        write_group_row(out, proto, &stats)?;
    }

    // Sequential: group by full protocol sequence (no double-counting).
    let sequential: Vec<&AuditRow> = successes
        .iter()
        .copied()
        .filter(|r| matches!(r.num_swaps, Some(n) if n > 1))
        .collect();
    if sequential.is_empty() {
        return Ok(());
    }
    writeln!(out, "\n## By protocol sequence — sequential routes")?;
    let mut by_seq: std::collections::BTreeMap<String, Vec<&AuditRow>> =
        std::collections::BTreeMap::new();
    for r in &sequential {
        if let Some(p) = r.protocols.as_deref() {
            by_seq
                .entry(p.to_string())
                .or_default()
                .push(*r);
        }
    }
    write_group_table_header(out)?;
    for (seq, rows) in &by_seq {
        let stats = group_stats(rows);
        write_group_row(out, seq, &stats)?;
    }
    Ok(())
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
        gas_price_wei
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0) /
            1e9
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
    successes.sort_by(|a, b| {
        b.error_eth
            .map(|e| e.abs())
            .unwrap_or(0.0)
            .partial_cmp(
                &a.error_eth
                    .map(|e| e.abs())
                    .unwrap_or(0.0),
            )
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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
            r.error_eth.unwrap_or(0.0)
        )?;
    }

    write_route_shape_breakdown(&mut out, &successes)?;
    write_protocol_breakdown(&mut out, &successes)?;

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

    fn row(status: RowStatus, error_eth: Option<f64>, actual: Option<u64>) -> AuditRow {
        AuditRow {
            token_in: "0xa".into(),
            token_out: "0xb".into(),
            amount_in: "1".into(),
            gas_estimate: Some(BigUint::from(200_000u64)),
            actual_gas: actual,
            gas_price_wei: BigUint::from(20_000_000_000u64),
            error_gas: None,
            error_wei: None,
            error_eth,
            status,
            error_reason: None,
            num_swaps: None,
            protocols: None,
        }
    }

    #[test]
    fn summarize_counts_statuses() {
        let rows = vec![
            row(RowStatus::Success, Some(0.001), Some(150_000)),
            row(RowStatus::Success, Some(-0.002), Some(220_000)),
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
        let rows = vec![row(RowStatus::Success, Some(0.001), Some(150_000))];
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
            rows.push(row(RowStatus::Success, Some(0.0), Some(200_000)));
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
            rows.push(row(RowStatus::Success, Some(0.0), Some(200_000)));
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
