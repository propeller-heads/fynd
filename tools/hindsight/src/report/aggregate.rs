//! Aggregate parsed comparison records into the numbers the report renders.
//!
//! Everything here is a pure function of `&[Comparison]`, judged on the headline (top-of-block)
//! state, so the aggregates match the monitor's headline verdict and are straightforward to test.

use std::collections::HashMap;

use crate::report::record::Comparison;

/// Number of order pairs listed in the mock-`PropAMM` breakdown table.
const TOP_PROPAMM_PAIRS: usize = 10;

/// Number of trades listed in the biggest-wins and biggest-losses tables.
const TOP_TRADES: usize = 10;
/// Number of tokens listed in the unsolvable-tail table.
const TOP_UNSOLVABLE_TOKENS: usize = 15;

/// Verdicts in display order — the order of the legend, the stacked columns (top down), and the
/// table. Win, unsolvable, then loss, so the win green and the loss red are never adjacent
/// segments: the two are indistinguishable under deuteranopia. Only verdicts present in the data
/// are rendered.
const VERDICT_ORDER: &[&str] = &["win", "unsolvable", "loss", "coverage_miss", "sandwiched"];

/// The fully aggregated report, ready to render.
pub(crate) struct Report {
    pub summary: Summary,
    pub verdicts: Vec<VerdictStat>,
    pub savings: Savings,
    pub by_solver: Vec<GroupStats>,
    pub by_venue: Vec<GroupStats>,
    pub top_wins: Vec<TradeRow>,
    pub top_losses: Vec<TradeRow>,
    pub unsolvable_tokens: Vec<Count>,
    /// Present only for a run the monitor drove with `--propamm-pair`.
    pub propamm: Option<PropAmm>,
}

/// The mock-`PropAMM` view: how often the exclusive route beat the public one, and what fee it
/// could have charged on the flow it took.
///
/// The run-wide winrate is over every solved order, most of which never touch the mirrored pair —
/// so `by_pair` is where the answer actually lives. Each row is one order direction, and the mock
/// can serve an order whose own pair differs from the mirrored one (a `DAI→USDC` order routed
/// `DAI→WETH→USDC` uses a `WETH/USDC` leg), which is exactly what the breakdown shows.
pub(crate) struct PropAmm {
    /// The mirrored pair as token symbols, e.g. `WETH/USDC`.
    pub pair: Option<String>,
    /// Orders with a scored quote — the winrate's denominator.
    pub solved: usize,
    /// Of those, how many routed through the mock pool.
    pub won: usize,
    /// Committed output on wins, valued in USD — the flow the pool captured.
    pub captured_flow_usd: f64,
    /// Fee headroom on wins, valued in USD.
    pub fee_headroom_usd: f64,
    /// Median fee headroom in bps over wins — the typical fee the pool could charge.
    pub median_headroom_bps: Option<f64>,
    /// Per-order-pair breakdown, most wins first.
    pub by_pair: Vec<PropAmmPair>,
}

impl PropAmm {
    /// Fee headroom as a fraction of captured flow, in basis points. `None` when it captured no
    /// flow, where the ratio is undefined rather than zero.
    pub(crate) fn avg_headroom_bps(&self) -> Option<f64> {
        (self.captured_flow_usd > 0.0)
            .then(|| self.fee_headroom_usd / self.captured_flow_usd * 10_000.0)
    }
}

/// One order direction in the mock-`PropAMM` breakdown.
pub(crate) struct PropAmmPair {
    /// `token_in` address.
    pub token_in: String,
    /// `token_out` address.
    pub token_out: String,
    /// Solved orders in this direction.
    pub solved: usize,
    /// Of those, how many the mock pool won.
    pub won: usize,
    /// Committed output on those wins, valued in USD.
    pub captured_flow_usd: f64,
    /// Fee headroom on those wins, valued in USD.
    pub fee_headroom_usd: f64,
    /// Median fee headroom in bps over those wins.
    pub median_headroom_bps: Option<f64>,
}

pub(crate) struct Summary {
    pub total: usize,
    pub distinct_blocks: usize,
}

/// A labelled count, for the token-tail table.
pub(crate) struct Count {
    pub label: String,
    pub count: usize,
}

/// One verdict, counted two ways: by trade count and by settled USD notional (volume). Both feed a
/// stacked bar, so the split can be read by number of trades and by dollars — a long tail of tiny
/// unsolvable trades is many trades but little volume.
pub(crate) struct VerdictStat {
    pub label: String,
    pub count: usize,
    pub notional_usd: f64,
}

/// Routing-quality view over scored (win/loss) trades. The losses are not summarised here — the
/// report lists each one instead, so a single bad-liquidity snapshot cannot pass for a trend.
pub(crate) struct Savings {
    pub scored: usize,
    pub wins: usize,
    /// Median net bps over winning trades — the typical savings when Fynd wins.
    pub median_win_bps: Option<f64>,
    /// USD gained on winning trades, the signed gross Fynd-vs-settled delta on wins (the
    /// `hindsight_savings_usd` metric).
    pub won_usd: f64,
}

/// Per-solver or per-venue breakdown row.
pub(crate) struct GroupStats {
    pub name: String,
    pub count: usize,
    pub wins: usize,
    pub losses: usize,
    pub unsolved: usize,
    pub median_net_bps: Option<f64>,
    pub total_improvement_usd: f64,
}

/// One row in the biggest-wins or biggest-losses table.
pub(crate) struct TradeRow {
    pub settled_tx: String,
    pub venue: String,
    pub solver: String,
    pub net_bps: Option<f64>,
    pub improvement_usd: f64,
}

/// Aggregate every view from the parsed records.
pub(crate) fn build(records: &[Comparison]) -> Report {
    Report {
        summary: summary(records),
        verdicts: verdict_stats(records),
        savings: savings(records),
        by_solver: group_stats(records, |r| &r.solver),
        by_venue: group_stats(records, |r| &r.venue),
        top_wins: top_wins(records),
        top_losses: top_losses(records),
        unsolvable_tokens: unsolvable_tokens(records),
        propamm: propamm(records),
    }
}

/// The mock-`PropAMM` view, or `None` when no record carries one — i.e. the monitor ran without
/// `--propamm-pair`, so the section is omitted rather than rendered empty.
///
/// Only records with a `propamm` field count toward `solved`: a run that enabled the harness
/// mid-way would otherwise dilute the winrate with trades the mock never saw.
fn propamm(records: &[Comparison]) -> Option<PropAmm> {
    let scoped: Vec<&Comparison> = records
        .iter()
        .filter(|r| r.propamm.is_some())
        .collect();
    if scoped.is_empty() {
        return None;
    }
    let wins: Vec<&Comparison> = scoped
        .iter()
        .copied()
        .filter(|r| {
            r.propamm
                .as_ref()
                .is_some_and(|p| p.won)
        })
        .collect();
    let mut headroom_bps: Vec<f64> = wins
        .iter()
        .filter_map(|r| {
            r.propamm
                .as_ref()
                .and_then(|p| p.fee_headroom_bps)
        })
        .collect();
    Some(PropAmm {
        pair: scoped.iter().find_map(|r| {
            r.propamm
                .as_ref()
                .and_then(|p| p.pair.clone())
        }),
        solved: scoped.len(),
        won: wins.len(),
        captured_flow_usd: sum_propamm(&wins, |p| p.committed_usd),
        fee_headroom_usd: sum_propamm(&wins, |p| p.fee_headroom_usd),
        median_headroom_bps: median(&mut headroom_bps),
        by_pair: propamm_by_pair(&scoped),
    })
}

/// Sums an optional USD field over records, skipping the ones where the token was not priced.
fn sum_propamm(
    records: &[&Comparison],
    field: impl Fn(&crate::report::record::PropAmm) -> Option<f64>,
) -> f64 {
    records
        .iter()
        .filter_map(|r| r.propamm.as_ref().and_then(&field))
        .filter(|value| value.is_finite())
        .sum()
}

/// Groups the mock-`PropAMM` outcomes by order direction, ranked by wins then by solved count, so
/// the pairs the exclusive route actually served come first.
fn propamm_by_pair(scoped: &[&Comparison]) -> Vec<PropAmmPair> {
    let mut grouped: HashMap<(&str, &str), Vec<&Comparison>> = HashMap::new();
    for record in scoped {
        grouped
            .entry((record.token_in.as_str(), record.token_out.as_str()))
            .or_default()
            .push(record);
    }

    let mut rows: Vec<PropAmmPair> = grouped
        .into_iter()
        .map(|((token_in, token_out), records)| {
            let wins: Vec<&Comparison> = records
                .iter()
                .copied()
                .filter(|r| {
                    r.propamm
                        .as_ref()
                        .is_some_and(|p| p.won)
                })
                .collect();
            let mut headroom_bps: Vec<f64> = wins
                .iter()
                .filter_map(|r| {
                    r.propamm
                        .as_ref()
                        .and_then(|p| p.fee_headroom_bps)
                })
                .collect();
            PropAmmPair {
                token_in: token_in.to_string(),
                token_out: token_out.to_string(),
                solved: records.len(),
                won: wins.len(),
                captured_flow_usd: sum_propamm(&wins, |p| p.committed_usd),
                fee_headroom_usd: sum_propamm(&wins, |p| p.fee_headroom_usd),
                median_headroom_bps: median(&mut headroom_bps),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.won
            .cmp(&a.won)
            .then_with(|| b.solved.cmp(&a.solved))
            .then_with(|| a.token_in.cmp(&b.token_in))
    });
    rows.truncate(TOP_PROPAMM_PAIRS);
    rows
}

fn summary(records: &[Comparison]) -> Summary {
    let mut blocks: Vec<u64> = records
        .iter()
        .map(|r| r.block)
        .collect();
    blocks.sort_unstable();
    blocks.dedup();
    Summary { total: records.len(), distinct_blocks: blocks.len() }
}

fn verdict_stats(records: &[Comparison]) -> Vec<VerdictStat> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut notional: HashMap<&str, f64> = HashMap::new();
    for record in records {
        let verdict = record.top.verdict.as_str();
        *counts.entry(verdict).or_default() += 1;
        *notional.entry(verdict).or_default() += record
            .top
            .settled_value_usd
            .unwrap_or(0.0);
    }
    let mut ordered = Vec::new();
    for &verdict in VERDICT_ORDER {
        if let Some(&count) = counts.get(verdict) {
            ordered.push(VerdictStat {
                label: verdict.to_string(),
                count,
                notional_usd: notional
                    .get(verdict)
                    .copied()
                    .unwrap_or(0.0),
            });
        }
    }
    ordered
}

fn savings(records: &[Comparison]) -> Savings {
    let scored: Vec<&Comparison> = records
        .iter()
        .filter(|r| r.top.is_scored())
        .collect();
    // The savings-bps headline is over wins only — how much better Fynd was when it won, not
    // diluted by the losses.
    let mut win_bps: Vec<f64> = scored
        .iter()
        .filter(|r| r.top.verdict == "win")
        .filter_map(|r| r.top.net_bps)
        .collect();
    Savings {
        scored: scored.len(),
        wins: scored
            .iter()
            .filter(|r| r.top.verdict == "win")
            .count(),
        median_win_bps: median(&mut win_bps),
        won_usd: scored
            .iter()
            .filter(|r| r.top.verdict == "win")
            .filter_map(|r| r.top.improvement_usd)
            .sum(),
    }
}

fn group_stats(records: &[Comparison], key: impl Fn(&Comparison) -> &String) -> Vec<GroupStats> {
    let mut groups: HashMap<&String, Vec<&Comparison>> = HashMap::new();
    for record in records {
        groups
            .entry(key(record))
            .or_default()
            .push(record);
    }
    let mut stats: Vec<GroupStats> = groups
        .into_iter()
        .map(|(name, group)| {
            let mut net_bps: Vec<f64> = group
                .iter()
                .filter(|r| r.top.is_scored())
                .filter_map(|r| r.top.net_bps)
                .collect();
            GroupStats {
                name: name.clone(),
                count: group.len(),
                wins: group
                    .iter()
                    .filter(|r| r.top.verdict == "win")
                    .count(),
                losses: group
                    .iter()
                    .filter(|r| r.top.verdict == "loss")
                    .count(),
                unsolved: group
                    .iter()
                    .filter(|r| !r.top.is_served())
                    .count(),
                median_net_bps: median(&mut net_bps),
                total_improvement_usd: group
                    .iter()
                    .filter(|r| r.top.is_scored())
                    .filter_map(|r| r.top.improvement_usd)
                    .sum(),
            }
        })
        .collect();
    stats.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.name.cmp(&b.name))
    });
    stats
}

fn top_wins(records: &[Comparison]) -> Vec<TradeRow> {
    let mut wins: Vec<TradeRow> = trade_rows(records, "win");
    wins.sort_by(|a, b| {
        b.improvement_usd
            .total_cmp(&a.improvement_usd)
    });
    wins.truncate(TOP_TRADES);
    wins
}

fn top_losses(records: &[Comparison]) -> Vec<TradeRow> {
    let mut losses: Vec<TradeRow> = trade_rows(records, "loss");
    losses.sort_by(|a, b| {
        a.improvement_usd
            .total_cmp(&b.improvement_usd)
    });
    losses.truncate(TOP_TRADES);
    losses
}

fn trade_rows(records: &[Comparison], verdict: &str) -> Vec<TradeRow> {
    records
        .iter()
        .filter(|r| r.top.verdict == verdict)
        .filter_map(|r| {
            r.top
                .improvement_usd
                .map(|usd| TradeRow {
                    settled_tx: r.settled_tx.clone(),
                    venue: r.venue.clone(),
                    solver: r.solver.clone(),
                    net_bps: r.top.net_bps,
                    improvement_usd: usd,
                })
        })
        .collect()
}

fn unsolvable_tokens(records: &[Comparison]) -> Vec<Count> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for record in records
        .iter()
        .filter(|r| !r.top.is_served())
    {
        *counts
            .entry(record.token_in.as_str())
            .or_default() += 1;
        *counts
            .entry(record.token_out.as_str())
            .or_default() += 1;
    }
    let mut ranked: Vec<Count> = counts
        .into_iter()
        .map(|(token, count)| Count { label: token.to_string(), count })
        .collect();
    ranked.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.label.cmp(&b.label))
    });
    ranked.truncate(TOP_UNSOLVABLE_TOKENS);
    ranked
}

/// Median of `values`, sorting in place. `None` when empty.
fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some(f64::midpoint(values[mid - 1], values[mid]))
    } else {
        Some(values[mid])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        block: u64,
        venue: &str,
        solver: &str,
        verdict: &str,
        bps: Option<f64>,
    ) -> Comparison {
        serde_json::from_value(serde_json::json!({
            "block": block,
            "settled_tx": format!("0x{block:064x}"),
            "venue": venue,
            "solver": solver,
            "token_in": "0xaaa",
            "token_out": "0xbbb",
            "top": {
                "verdict": verdict,
                "net_bps": bps,
                "improvement_usd": bps.map(|b| b / 10.0),
                "settled_value_usd": 1000.0,
            },
        }))
        .unwrap()
    }

    #[test]
    fn test_verdict_stats_in_display_order_with_volume() {
        let records = vec![
            record(1, "relay", "1inch", "unsolvable", None),
            record(2, "relay", "1inch", "win", Some(20.0)),
            record(3, "relay", "0x", "win", Some(5.0)),
            record(4, "relay", "0x", "loss", Some(-8.0)),
        ];
        let stats = verdict_stats(&records);
        let labels: Vec<&str> = stats
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, vec!["win", "unsolvable", "loss"]);
        assert_eq!(stats[0].count, 2);
        // Each record's settled_value_usd is 1000; two wins → 2000 volume.
        assert!((stats[0].notional_usd - 2000.0).abs() < 1e-6);
    }

    #[test]
    fn test_savings_median_is_wins_only_and_excludes_sandwiched() {
        let records = vec![
            record(1, "relay", "1inch", "win", Some(20.0)),
            record(2, "relay", "1inch", "loss", Some(-10.0)),
            record(3, "relay", "1inch", "sandwiched", Some(500.0)),
        ];
        let savings = savings(&records);
        // The loss is scored, the sandwiched trade is not.
        assert_eq!(savings.scored, 2);
        assert_eq!(savings.wins, 1);
        // Median over wins only: [20]; the loss and sandwiched are excluded.
        assert!((savings.median_win_bps.unwrap() - 20.0).abs() < 1e-6);
        // Won +2.0 on the one win; the loss and the sandwiched 500 bps do not enter it.
        assert!((savings.won_usd - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_top_wins_and_losses_ordered_by_usd() {
        let records = vec![
            record(1, "relay", "1inch", "win", Some(10.0)),
            record(2, "relay", "0x", "win", Some(80.0)),
            record(3, "relay", "0x", "loss", Some(-90.0)),
            record(4, "relay", "1inch", "loss", Some(-5.0)),
        ];
        let wins = top_wins(&records);
        assert_eq!(wins[0].solver, "0x");
        assert!(wins[0].improvement_usd > wins[1].improvement_usd);
        let losses = top_losses(&records);
        assert!(losses[0].improvement_usd < 0.0);
        assert!(losses[0].improvement_usd < losses[1].improvement_usd);
    }

    #[test]
    fn test_group_stats_sorted_by_count() {
        let records = vec![
            record(1, "relay", "1inch", "win", Some(10.0)),
            record(2, "relay", "1inch", "loss", Some(-5.0)),
            record(3, "metamask", "0x", "unsolvable", None),
        ];
        let by_venue = group_stats(&records, |r| &r.venue);
        assert_eq!(by_venue[0].name, "relay");
        assert_eq!(by_venue[0].count, 2);
        assert_eq!(by_venue[0].wins, 1);
        assert_eq!(by_venue[1].name, "metamask");
        assert_eq!(by_venue[1].unsolved, 1);
    }

    #[test]
    fn test_median_even_and_odd() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }

    /// A record carrying a mock-`PropAMM` outcome.
    fn propamm_record(
        block: u64,
        token_in: &str,
        token_out: &str,
        won: bool,
        headroom_bps: f64,
        committed_usd: f64,
    ) -> Comparison {
        serde_json::from_value(serde_json::json!({
            "block": block,
            "settled_tx": format!("0x{block:064x}"),
            "venue": "relay",
            "solver": "1inch",
            "token_in": token_in,
            "token_out": token_out,
            "top": { "verdict": "win", "net_bps": 5.0, "settled_value_usd": 1000.0 },
            "propamm": {
                "pair": "WETH/USDC",
                "won": won,
                "fee_headroom_bps": won.then_some(headroom_bps),
                "committed_usd": won.then_some(committed_usd),
                "fee_headroom_usd": won.then(|| committed_usd * headroom_bps / 10_000.0),
            },
        }))
        .unwrap()
    }

    #[test]
    fn test_propamm_absent_without_the_harness() {
        // An ordinary monitor run writes no `propamm` field, so the section must be omitted rather
        // than rendered as a run where the pool won nothing.
        let records = vec![record(1, "relay", "1inch", "win", Some(5.0))];
        assert!(propamm(&records).is_none());
    }

    #[test]
    fn test_propamm_counts_only_records_the_harness_saw() {
        // Enabling the harness mid-run must not dilute the winrate with trades the mock never saw.
        let records = vec![
            record(1, "relay", "1inch", "win", Some(5.0)),
            propamm_record(2, "0xweth", "0xusdc", true, 4.0, 1_000.0),
            propamm_record(3, "0xweth", "0xusdc", false, 0.0, 0.0),
        ];
        let propamm = propamm(&records).expect("some records carry an outcome");
        assert_eq!(propamm.solved, 2);
        assert_eq!(propamm.won, 1);
        assert_eq!(propamm.pair.as_deref(), Some("WETH/USDC"));
    }

    #[test]
    fn test_propamm_totals_and_flow_weighted_headroom() {
        let records = vec![
            propamm_record(1, "0xweth", "0xusdc", true, 4.0, 1_000.0),
            propamm_record(2, "0xweth", "0xusdc", true, 8.0, 3_000.0),
            propamm_record(3, "0xweth", "0xusdc", false, 0.0, 0.0),
        ];
        let propamm = propamm(&records).expect("outcomes present");

        assert!((propamm.captured_flow_usd - 4_000.0).abs() < 1e-6);
        // 1000 @ 4 bps = 0.40, 3000 @ 8 bps = 2.40.
        assert!((propamm.fee_headroom_usd - 2.8).abs() < 1e-6);
        assert!((propamm.median_headroom_bps.unwrap() - 6.0).abs() < 1e-6);
        // Flow-weighted, not the mean of the two bps values: 2.8 / 4000 = 7 bps.
        assert!((propamm.avg_headroom_bps().unwrap() - 7.0).abs() < 1e-6);
    }

    #[test]
    fn test_propamm_avg_headroom_undefined_without_captured_flow() {
        // No flow means the ratio has no denominator; reporting 0 would read as "no headroom".
        let records = vec![propamm_record(1, "0xweth", "0xusdc", false, 0.0, 0.0)];
        let propamm = propamm(&records).expect("outcomes present");
        assert!(propamm.avg_headroom_bps().is_none());
    }

    #[test]
    fn test_propamm_by_pair_ranks_the_served_pairs_first() {
        // The mirrored pair is WETH/USDC, but the mock also serves a DAI→USDC order routed through
        // WETH. The pair with wins must outrank an equally-sized pair without any.
        let records = vec![
            propamm_record(1, "0xdai", "0xusdc", false, 0.0, 0.0),
            propamm_record(2, "0xdai", "0xusdc", false, 0.0, 0.0),
            propamm_record(3, "0xweth", "0xusdc", true, 4.0, 1_000.0),
            propamm_record(4, "0xweth", "0xusdc", false, 0.0, 0.0),
        ];
        let propamm = propamm(&records).expect("outcomes present");
        let rows = &propamm.by_pair;

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].token_in, "0xweth");
        assert_eq!(rows[0].solved, 2);
        assert_eq!(rows[0].won, 1);
        assert_eq!(rows[1].token_in, "0xdai");
        assert_eq!(rows[1].won, 0);
        assert!(rows[1].median_headroom_bps.is_none());
    }
}
