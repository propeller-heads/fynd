//! Aggregate parsed comparison records into the numbers the report renders.
//!
//! Everything here is a pure function of `&[Comparison]`, judged on the headline (top-of-block)
//! state, so the aggregates match the monitor's headline verdict and are straightforward to test.

use std::collections::HashMap;

use crate::report::record::Comparison;

/// Slack allowed when checking a group's fee against its offset, in basis points.
///
/// The calibration divides integer amounts and the source pool rounds its own output, so a fee
/// lands a fraction of a bps off its target. Anything beyond this is a real discrepancy, not
/// rounding.
const FEE_TOLERANCE_BPS: f64 = 0.5;

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

/// The mock-`PropAMM` view: what the pool captured, whether each price group behaved, and the
/// with/without comparison.
pub(crate) struct PropAmm {
    /// The mirrored pair as token symbols, e.g. `WETH/USDC`.
    pub pair: Option<String>,
    /// Committed output on wins, valued in USD — the flow the pool captured.
    pub captured_flow_usd: f64,
    /// Fee headroom on wins, valued in USD.
    pub fee_headroom_usd: f64,
    /// Per-offset groups, ascending. Empty for a run with no calibrated orders.
    pub groups: Vec<PropAmmGroup>,
    /// The same orders scored with and without the mock — what the pool actually bought us.
    pub uplift: Uplift,
}

/// The controlled A/B: the same orders, at the same block states, solved with the mock available
/// and with it neutralised.
///
/// This is the only place the report answers "did the `PropAMM` help", because it is the only
/// comparison where nothing else varies. It covers calibrated orders alone — an off-pair order has
/// no "without" pass, since the mock could never have served it.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Uplift {
    /// Orders scored in both worlds.
    pub orders: usize,
    /// Of those, how many Fynd won without the mock.
    pub wins_without: usize,
    /// Of those, how many Fynd won with the mock.
    pub wins_with: usize,
    /// USD gained over the settled trades without the mock, summed over winning orders.
    pub profit_without_usd: f64,
    /// USD gained over the settled trades with the mock, summed over winning orders.
    pub profit_with_usd: f64,
    /// Median net-of-gas bps over winning orders without the mock.
    pub median_bps_without: Option<f64>,
    /// Median net-of-gas bps over winning orders with the mock.
    pub median_bps_with: Option<f64>,
}

impl Uplift {
    /// Win rate without the mock, as a percentage. Zero when nothing was scored.
    // Precision loss is irrelevant: these are order counts, far below f64's exact-integer range.
    #[expect(clippy::cast_precision_loss)]
    pub(crate) fn winrate_without_pct(&self) -> f64 {
        if self.orders == 0 {
            return 0.0;
        }
        self.wins_without as f64 / self.orders as f64 * 100.0
    }

    /// Win rate with the mock, as a percentage. Zero when nothing was scored.
    // Precision loss is irrelevant: these are order counts, far below f64's exact-integer range.
    #[expect(clippy::cast_precision_loss)]
    pub(crate) fn winrate_with_pct(&self) -> f64 {
        if self.orders == 0 {
            return 0.0;
        }
        self.wins_with as f64 / self.orders as f64 * 100.0
    }

    /// Extra orders won because the mock was there.
    pub(crate) fn extra_wins(&self) -> i64 {
        i64::try_from(self.wins_with).unwrap_or(i64::MAX) -
            i64::try_from(self.wins_without).unwrap_or(i64::MAX)
    }

    /// Extra USD earned because the mock was there.
    pub(crate) fn extra_profit_usd(&self) -> f64 {
        self.profit_with_usd - self.profit_without_usd
    }
}

impl PropAmm {
    /// The run's overall verdict: the worst of its groups.
    ///
    /// One failing group fails the run — a violation at any price is a violation, and averaging it
    /// against the groups that passed would bury it.
    pub(crate) fn verdict(&self) -> GroupVerdict {
        if let Some(failure) = self
            .groups
            .iter()
            .find_map(|group| match &group.verdict {
                GroupVerdict::Fail(reason) => Some(reason.clone()),
                GroupVerdict::Pass | GroupVerdict::NoData => None,
            })
        {
            return GroupVerdict::Fail(failure);
        }
        if self
            .groups
            .iter()
            .any(|group| group.verdict == GroupVerdict::Pass)
        {
            GroupVerdict::Pass
        } else {
            GroupVerdict::NoData
        }
    }
}

/// Whether an offset group behaved as its price implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GroupVerdict {
    /// The group met its expectation.
    Pass,
    /// The group violated it; the string says how.
    Fail(String),
    /// No calibrated order landed in this group, so there is nothing to conclude.
    NoData,
}

/// One offset group: orders priced the same distance from the public best route, and whether the
/// router treated them the way that price implies.
///
/// This is the harness's actual test. Each order's competitive situation is constructed, so the
/// group is an assertion rather than a measurement:
///
/// - **below the market** (`offset_bps < 0`) — the mock is strictly worse, so it must never be
///   selected;
/// - **at the market** (`offset_bps == 0`) — it can only win on gas, and then there is no surplus,
///   so any selection must carry a zero fee;
/// - **above the market** (`offset_bps > 0`) — the fee taken cannot exceed the offset, because the
///   offset is all the surplus there is.
pub(crate) struct PropAmmGroup {
    /// The offset these orders were priced at, in basis points off the public best route.
    pub offset_bps: i32,
    /// Calibrated orders in this group.
    pub orders: usize,
    /// Of those, how many the router routed through the mock.
    pub selected: usize,
    /// Largest fee taken in the group, in bps — the number the expectation is checked against.
    pub max_fee_bps: Option<f64>,
    /// Median fee taken, in bps.
    pub median_fee_bps: Option<f64>,
    /// Whether the group met its expectation.
    pub verdict: GroupVerdict,
}

impl PropAmmGroup {
    /// The expectation this group's price implies, as a short phrase for the report.
    pub(crate) fn expectation(&self) -> &'static str {
        match self.offset_bps.cmp(&0) {
            std::cmp::Ordering::Less => "never selected",
            std::cmp::Ordering::Equal => "selected only on gas, zero fee",
            std::cmp::Ordering::Greater => "selected, fee at most the offset",
        }
    }

    /// Share of the group's orders the mock was selected for, as a percentage.
    // Precision loss is irrelevant: these are order counts, far below f64's exact-integer range.
    #[expect(clippy::cast_precision_loss)]
    pub(crate) fn selected_pct(&self) -> f64 {
        if self.orders == 0 {
            return 0.0;
        }
        self.selected as f64 / self.orders as f64 * 100.0
    }
}

impl PropAmm {
    /// Fee headroom as a fraction of captured flow, in basis points. `None` when it captured no
    /// flow, where the ratio is undefined rather than zero.
    pub(crate) fn avg_headroom_bps(&self) -> Option<f64> {
        (self.captured_flow_usd > 0.0)
            .then(|| self.fee_headroom_usd / self.captured_flow_usd * 10_000.0)
    }
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
/// mid-way would otherwise dilute the winrate with trades the mock never saw. The monitor writes
/// the field for every *solved* order, win or lose, so the field's presence is the denominator.
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
    Some(PropAmm {
        pair: scoped.iter().find_map(|r| {
            r.propamm
                .as_ref()
                .and_then(|p| p.pair.clone())
        }),
        captured_flow_usd: sum_propamm(&wins, |p| p.committed_usd),
        fee_headroom_usd: sum_propamm(&wins, |p| p.fee_headroom_usd),
        groups: propamm_groups(&scoped),
        uplift: uplift(&scoped),
    })
}

/// Sums the with/without A/B over the orders that carry both sides.
///
/// Wins and profits are counted independently: an order can be scored for its verdict but have an
/// unpriced output token, so it contributes to the win counts and not to the USD.
fn uplift(scoped: &[&Comparison]) -> Uplift {
    let mut uplift = Uplift::default();
    let mut bps_without: Vec<f64> = Vec::new();
    let mut bps_with: Vec<f64> = Vec::new();
    for propamm in scoped
        .iter()
        .filter_map(|r| r.propamm.as_ref())
    {
        let (Some(without_won), Some(with_won)) = (propamm.without_won, propamm.with_won) else {
            continue;
        };
        uplift.orders += 1;
        uplift.wins_without += usize::from(without_won);
        uplift.wins_with += usize::from(with_won);
        // Only winning orders contribute profit: a loss is not negative revenue, it is a trade Fynd
        // would not have served, which is what the win counts already say.
        if without_won {
            if let Some(usd) = propamm
                .without_improvement_usd
                .filter(|usd| usd.is_finite())
            {
                uplift.profit_without_usd += usd;
            }
        }
        if with_won {
            if let Some(usd) = propamm
                .with_improvement_usd
                .filter(|usd| usd.is_finite())
            {
                uplift.profit_with_usd += usd;
            }
        }
        // The bps headline is over wins only, matching the report's own median: how much better
        // Fynd was when it won, not diluted by the trades it lost.
        if without_won {
            bps_without.extend(propamm.without_net_bps);
        }
        if with_won {
            bps_with.extend(propamm.with_net_bps);
        }
    }
    uplift.median_bps_without = median(&mut bps_without);
    uplift.median_bps_with = median(&mut bps_with);
    uplift
}

/// Groups the calibrated orders by offset and judges each group against its price.
///
/// Orders with no offset are excluded: they were not calibrated, so no expectation applies to them.
fn propamm_groups(scoped: &[&Comparison]) -> Vec<PropAmmGroup> {
    let mut grouped: HashMap<i32, Vec<&Comparison>> = HashMap::new();
    for record in scoped {
        if let Some(offset) = record
            .propamm
            .as_ref()
            .and_then(|p| p.offset_bps)
        {
            grouped
                .entry(offset)
                .or_default()
                .push(record);
        }
    }

    let mut groups: Vec<PropAmmGroup> = grouped
        .into_iter()
        .map(|(offset_bps, records)| {
            let mut fees: Vec<f64> = records
                .iter()
                .filter(|r| {
                    r.propamm
                        .as_ref()
                        .is_some_and(|p| p.won)
                })
                .filter_map(|r| {
                    r.propamm
                        .as_ref()
                        .and_then(|p| p.fee_headroom_bps)
                })
                .collect();
            let selected = records
                .iter()
                .filter(|r| {
                    r.propamm
                        .as_ref()
                        .is_some_and(|p| p.won)
                })
                .count();
            let max_fee_bps = fees
                .iter()
                .copied()
                .fold(None::<f64>, |acc, fee| Some(acc.map_or(fee, |best| best.max(fee))));
            let mut group = PropAmmGroup {
                offset_bps,
                orders: records.len(),
                selected,
                max_fee_bps,
                median_fee_bps: median(&mut fees),
                verdict: GroupVerdict::NoData,
            };
            group.verdict = judge_group(&group);
            group
        })
        .collect();
    groups.sort_by_key(|group| group.offset_bps);
    groups
}

/// Judges one offset group against the expectation its price implies.
fn judge_group(group: &PropAmmGroup) -> GroupVerdict {
    if group.orders == 0 {
        return GroupVerdict::NoData;
    }
    if group.offset_bps < 0 {
        return if group.selected == 0 {
            GroupVerdict::Pass
        } else {
            GroupVerdict::Fail(format!(
                "priced {} bps below the public market, yet selected for {} of {} orders",
                -group.offset_bps, group.selected, group.orders
            ))
        };
    }
    if group.selected == 0 {
        // Not a failure at zero offset — the mock only wins there on gas, and it need not be
        // cheaper. Above zero it should win, so say so without calling the run broken.
        return GroupVerdict::NoData;
    }
    let Some(max_fee) = group.max_fee_bps else {
        return GroupVerdict::NoData;
    };
    let ceiling = f64::from(group.offset_bps) + FEE_TOLERANCE_BPS;
    if max_fee <= ceiling {
        GroupVerdict::Pass
    } else {
        GroupVerdict::Fail(format!(
            "priced {} bps above the public market, yet took a fee of {max_fee:.2} bps",
            group.offset_bps
        ))
    }
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
        assert_eq!(propamm.pair.as_deref(), Some("WETH/USDC"));
        // Both records carry an outcome, so both are in scope; only one is a win.
        assert!((propamm.captured_flow_usd - 1_000.0).abs() < 1e-6);
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

    /// A calibrated record: priced `offset_bps` off the public best route, selected or not.
    fn calibrated(block: u64, offset_bps: i32, won: bool, fee_bps: f64) -> Comparison {
        serde_json::from_value(serde_json::json!({
            "block": block,
            "settled_tx": format!("0x{block:064x}"),
            "venue": "relay",
            "solver": "1inch",
            "token_in": "0xweth",
            "token_out": "0xusdt",
            "top": { "verdict": "win", "net_bps": 5.0, "settled_value_usd": 1000.0 },
            "propamm": {
                "pair": "WETH/USDT",
                "offset_bps": offset_bps,
                "won": won,
                "fee_headroom_bps": won.then_some(fee_bps),
                "committed_usd": won.then_some(1_000.0),
                "fee_headroom_usd": won.then(|| fee_bps / 10_000.0 * 1_000.0),
            },
        }))
        .unwrap()
    }

    #[test]
    fn test_groups_are_ordered_by_price_ascending() {
        let records = vec![
            calibrated(1, 5, true, 5.0),
            calibrated(2, -5, false, 0.0),
            calibrated(3, 0, true, 0.0),
        ];
        let groups = propamm(&records)
            .expect("outcomes present")
            .groups;
        let offsets: Vec<i32> = groups
            .iter()
            .map(|g| g.offset_bps)
            .collect();
        assert_eq!(offsets, vec![-5, 0, 5], "cards read below-market to above-market");
    }

    #[test]
    fn test_below_market_group_passes_only_when_never_selected() {
        let never = vec![calibrated(1, -5, false, 0.0), calibrated(2, -5, false, 0.0)];
        assert_eq!(propamm(&never).unwrap().groups[0].verdict, GroupVerdict::Pass);

        // A single selection below the public market breaks the router's core promise, so the group
        // must fail loudly rather than average the violation away.
        let once = vec![calibrated(1, -5, false, 0.0), calibrated(2, -5, true, 3.0)];
        let verdict = &propamm(&once).unwrap().groups[0].verdict;
        assert!(
            matches!(verdict, GroupVerdict::Fail(reason) if reason.contains("below the public")),
            "expected a failure naming the violation, got {verdict:?}"
        );
    }

    #[test]
    fn test_at_market_group_passes_on_a_zero_fee() {
        // At the market the mock can only win on gas, and then there is no surplus to take.
        let records = vec![calibrated(1, 0, true, 0.0), calibrated(2, 0, false, 0.0)];
        let group = &propamm(&records).unwrap().groups[0];
        assert_eq!(group.verdict, GroupVerdict::Pass);
        assert_eq!(group.selected, 1);
    }

    #[test]
    fn test_at_market_group_fails_on_a_non_zero_fee() {
        // Charging a fee at the market price means the user was quoted below the public route.
        let records = vec![calibrated(1, 0, true, 4.0)];
        assert!(matches!(&propamm(&records).unwrap().groups[0].verdict, GroupVerdict::Fail(_)));
    }

    #[test]
    fn test_above_market_group_caps_the_fee_at_the_offset() {
        // The offset is all the surplus there is, so a fee within it passes...
        let within = vec![calibrated(1, 5, true, 4.8), calibrated(2, 5, true, 5.0)];
        assert_eq!(propamm(&within).unwrap().groups[0].verdict, GroupVerdict::Pass);

        // ...and a fee beyond it means the pool signed more than it could charge.
        let beyond = vec![calibrated(1, 5, true, 5.0), calibrated(2, 5, true, 9.0)];
        let verdict = &propamm(&beyond).unwrap().groups[0].verdict;
        assert!(
            matches!(verdict, GroupVerdict::Fail(reason) if reason.contains("9.00")),
            "expected the offending fee in the message, got {verdict:?}"
        );
    }

    #[test]
    fn test_above_market_fee_tolerance_absorbs_rounding_only() {
        // Integer division puts a fee a hair over its target; that is rounding, not a discrepancy.
        let rounding = vec![calibrated(1, 5, true, 5.0 + FEE_TOLERANCE_BPS / 2.0)];
        assert_eq!(propamm(&rounding).unwrap().groups[0].verdict, GroupVerdict::Pass);

        let real = vec![calibrated(1, 5, true, 5.0 + FEE_TOLERANCE_BPS * 4.0)];
        assert!(matches!(&propamm(&real).unwrap().groups[0].verdict, GroupVerdict::Fail(_)));
    }

    #[test]
    fn test_group_with_no_selection_above_market_is_no_data_not_failure() {
        // Winning above the market depends on gas as well as price, so never winning is
        // inconclusive rather than a violation — the harness must not cry wolf.
        let records = vec![calibrated(1, 5, false, 0.0)];
        assert_eq!(propamm(&records).unwrap().groups[0].verdict, GroupVerdict::NoData);
    }

    #[test]
    fn test_uncalibrated_orders_form_no_group() {
        // Off-pair orders carry no offset, so no expectation applies and they must not dilute a
        // group.
        let records = vec![
            propamm_record(1, "0xdai", "0xusdc", false, 0.0, 0.0),
            calibrated(2, 5, true, 5.0),
        ];
        let propamm = propamm(&records).expect("outcomes present");
        assert_eq!(propamm.groups.len(), 1);
        assert_eq!(propamm.groups[0].orders, 1);
    }

    #[test]
    fn test_selected_pct_is_zero_rather_than_nan_when_empty() {
        let group = PropAmmGroup {
            offset_bps: 0,
            orders: 0,
            selected: 0,
            max_fee_bps: None,
            median_fee_bps: None,
            verdict: GroupVerdict::NoData,
        };
        assert!(group.selected_pct().abs() < f64::EPSILON);
    }

    /// A calibrated record carrying the with/without A/B.
    fn ab_record(
        block: u64,
        without_won: bool,
        with_won: bool,
        without_usd: f64,
        with_usd: f64,
    ) -> Comparison {
        serde_json::from_value(serde_json::json!({
            "block": block,
            "settled_tx": format!("0x{block:064x}"),
            "venue": "relay", "solver": "1inch",
            "token_in": "0xeth", "token_out": "0xusdc",
            "top": { "verdict": "win", "net_bps": 5.0, "settled_value_usd": 1000.0 },
            "propamm": {
                "pair": "ETH/USDC", "offset_bps": 5, "won": with_won && !without_won,
                "without_won": without_won, "with_won": with_won,
                "without_improvement_usd": without_usd, "with_improvement_usd": with_usd,
            },
        }))
        .unwrap()
    }

    #[test]
    fn test_uplift_counts_wins_in_both_worlds() {
        let records = vec![
            ab_record(1, false, true, -2.0, 3.0), // the mock turned a loss into a win
            ab_record(2, true, true, 4.0, 6.0),   // already won; the mock made it worth more
            ab_record(3, false, false, -1.0, -1.0), // the mock could not help
        ];
        let uplift = propamm(&records).unwrap().uplift;

        assert_eq!(uplift.orders, 3);
        assert_eq!(uplift.wins_without, 1);
        assert_eq!(uplift.wins_with, 2);
        assert_eq!(uplift.extra_wins(), 1);
    }

    #[test]
    fn test_uplift_profit_counts_winning_orders_only() {
        // A loss is not negative revenue — it is a trade Fynd would not have served, which the win
        // counts already express. Summing losing orders' USD would understate both columns.
        let records =
            vec![ab_record(1, false, true, -2.0, 3.0), ab_record(2, true, true, 4.0, 6.0)];
        let uplift = propamm(&records).unwrap().uplift;

        assert!(
            (uplift.profit_without_usd - 4.0).abs() < 1e-9,
            "the lost order contributes nothing"
        );
        assert!((uplift.profit_with_usd - 9.0).abs() < 1e-9);
        assert!((uplift.extra_profit_usd() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_uplift_winrates_are_over_the_same_orders() {
        let records = vec![
            ab_record(1, false, true, 0.0, 1.0),
            ab_record(2, false, true, 0.0, 1.0),
            ab_record(3, false, false, 0.0, 0.0),
            ab_record(4, false, false, 0.0, 0.0),
        ];
        let uplift = propamm(&records).unwrap().uplift;
        assert!(uplift.winrate_without_pct().abs() < f64::EPSILON);
        assert!((uplift.winrate_with_pct() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn test_uplift_skips_orders_scored_in_only_one_world() {
        // An off-pair order has no "without" pass; including it would inflate the denominator with
        // orders the mock could never have served.
        let records = vec![
            propamm_record(1, "0xdai", "0xusdc", false, 0.0, 0.0),
            ab_record(2, false, true, 0.0, 2.0),
        ];
        let uplift = propamm(&records).unwrap().uplift;
        assert_eq!(uplift.orders, 1);
    }

    #[test]
    fn test_uplift_reports_an_absolute_gain_over_a_zero_baseline() {
        // The public side earning nothing is not a divide-by-zero problem; the delta still stands.
        let records = vec![ab_record(1, false, true, 0.0, 5.0)];
        let uplift = propamm(&records).unwrap().uplift;
        assert!((uplift.extra_profit_usd() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_uplift_reports_a_regression_as_negative() {
        // If the mock ever made things worse, the numbers must say so rather than clamp at zero.
        let records = vec![ab_record(1, true, false, 5.0, 0.0)];
        let uplift = propamm(&records).unwrap().uplift;
        assert_eq!(uplift.extra_wins(), -1);
        assert!(uplift.extra_profit_usd() < 0.0);
    }

    #[test]
    fn test_empty_uplift_reports_zero_rather_than_nan() {
        let uplift = Uplift::default();
        assert!(uplift.winrate_without_pct().abs() < f64::EPSILON);
        assert!(uplift.winrate_with_pct().abs() < f64::EPSILON);
        assert_eq!(uplift.extra_wins(), 0);
    }
}
