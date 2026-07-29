//! Render an aggregated [`Report`] to a single self-contained HTML file.
//!
//! No external assets or network: the verdict split is a flexbox stacked column, styling is one
//! inline stylesheet, so the file opens offline. The panels mirror the dashboard's value views —
//! the headline Fynd savings (`hindsight_savings_usd`), the verdict split, coverage by notional,
//! per-solver/venue breakdowns, and the top-saving trades — and skip the block/latency health
//! panels the JSONL does not carry.

use std::fmt::Write as _;

use crate::report::aggregate::{
    Count, GroupStats, GroupVerdict, PropAmm, PropAmmGroup, PropAmmPair, Report, Savings, Summary,
    TradeRow, VerdictStat,
};

/// Shortest share of a stacked column that gets an inline `12.3%` label. Below it the segment is
/// not tall enough to hold a line of text with padding above and below, so the value stays in the
/// hover title and the table.
const MIN_LABELLED_SHARE: f64 = 8.0;

/// Colours for each verdict, shared by the stacked-column segments and their legend swatches. These
/// are status colours, not series identity: they carry good → critical in the same hues the Grafana
/// dashboard uses, so the report reads like the dashboard it mirrors. Green and red are
/// indistinguishable under deuteranopia, which is why every segment is also named in the legend and
/// listed in the table.
fn verdict_color(verdict: &str) -> &'static str {
    match verdict {
        "win" => "#43a047",
        "loss" => "#e53935",
        "unsolvable" => "#fbc02d",
        "coverage_miss" => "#fb8c00",
        "sandwiched" => "#ab47bc",
        _ => "#9e9e9e",
    }
}

/// Ink for a label sitting inside a verdict's fill, picked by the fill's luminance so the text
/// clears contrast against it.
fn verdict_ink(verdict: &str) -> &'static str {
    match verdict {
        "win" | "loss" | "sandwiched" => "#ffffff",
        _ => "#17111f",
    }
}

/// A verdict's display name: the JSONL's `coverage_miss` reads as `coverage miss`.
fn verdict_name(verdict: &str) -> String {
    verdict.replace('_', " ")
}

/// Render the whole report to an HTML document. `filter` names the active venue filter, if any, so
/// the report says which slice of trades it covers.
pub(crate) fn render(report: &Report, filter: Option<&str>) -> String {
    let mut html = String::from(HEAD);
    // The PropAMM verdict leads when there is one: a calibrated run exists to answer that question,
    // and everything below it is context.
    if let Some(propamm) = report.propamm.as_ref() {
        html.push_str(&propamm_section(propamm));
    }
    html.push_str(&hero_section(&report.savings, &report.summary, filter));
    html.push_str(&verdict_section(&report.verdicts));
    html.push_str(&trades_section("Top savings", &report.top_wins));
    html.push_str(&trades_section("Biggest losses", &report.top_losses));
    html.push_str(&group_section("By solver", &report.by_solver));
    html.push_str(&group_section("By venue", &report.by_venue));
    html.push_str(&token_section(&report.unsolvable_tokens));
    html.push_str(FOOT);
    html
}

/// The headline: Fynd's savings as the uplift on trades it wins (`hindsight_improvement_usd`), its
/// win rate, and its median savings when it wins. Underneath, the size of the run as plain counts.
/// The losses are not summarised here — each one is listed in the biggest-losses table.
fn hero_section(savings: &Savings, summary: &Summary, filter: Option<&str>) -> String {
    let scope = filter.map_or_else(
        || "<span class=\"chip\">all venues</span>".to_string(),
        |venue| format!("<span class=\"chip on\">venue: {}</span>", escape(venue)),
    );
    let big = |value: &str, label: &str, cls: &str| {
        format!(
            "<div class=\"herostat\"><div class=\"heronum {cls}\">{value}</div>\
             <div class=\"herolab\">{label}</div></div>"
        )
    };
    let mini = |value: &str, label: &str| {
        format!(
            "<div class=\"ministat\"><div class=\"mininum\">{value}</div>\
             <div class=\"minilab\">{label}</div></div>"
        )
    };
    format!(
        "<section class=\"hero\">\
           <div class=\"heroscope\">hindsight report {scope}</div>\
           <div class=\"herostats\">{}{}{}</div>\
           <div class=\"herofoot\">{}{}{}</div>\
         </section>",
        big(&fmt_usd(savings.won_usd), "Fynd savings (wins uplift)", "pos big"),
        big(&pct(savings.wins, savings.scored), "win rate", "pos"),
        big(&fmt_bps_signed(savings.median_win_bps), "median savings bps (wins)", "pos"),
        mini(&fmt_count(summary.distinct_blocks), "blocks"),
        mini(&fmt_count(summary.total), "comparisons"),
        mini(&fmt_count(savings.scored), "scored"),
    )
}

/// The verdict split as two 100% stacked columns — one weighted by trade count, one by settled USD
/// volume — sharing one legend, mirroring the dashboard's outcome-by-count and outcome-by-volume
/// panels. Both columns are stated again as a table, so every value is readable without reading a
/// colour.
fn verdict_section(verdicts: &[VerdictStat]) -> String {
    let by_count: Vec<(&str, f64)> = verdicts
        .iter()
        .map(|v| (v.label.as_str(), ratio(v.count, 1)))
        .collect();
    let by_volume: Vec<(&str, f64)> = verdicts
        .iter()
        .map(|v| (v.label.as_str(), v.notional_usd))
        .collect();
    let body = format!(
        "{}<div class=\"cols\">{}{}</div>{}",
        verdict_legend(verdicts),
        stacked_column("by trade count", &by_count, |v| format!("{v:.0}")),
        stacked_column("by volume (USD)", &by_volume, fmt_usd),
        verdict_table(verdicts),
    );
    section("Verdicts (top-of-block)", &body)
}

/// The legend both columns share: identity never rests on colour alone, so every verdict present in
/// the data is named beside its swatch.
fn verdict_legend(verdicts: &[VerdictStat]) -> String {
    let mut items = String::new();
    for verdict in verdicts {
        let _ = write!(
            items,
            "<li><span class=\"swatch\" style=\"background:{}\"></span>\
             <span class=\"legname\">{}</span></li>",
            verdict_color(&verdict.label),
            escape(&verdict_name(&verdict.label)),
        );
    }
    format!("<ul class=\"legend row\">{items}</ul>")
}

/// One 100% stacked column from `(verdict, value)` segments, captioned underneath. `fmt_val`
/// formats a segment's value for its hover title. Segments stack top down in the verdict order and
/// are separated by a 2px gap in the surface colour rather than a border; only a segment at least
/// [`MIN_LABELLED_SHARE`] tall carries an inline share label, so no label is ever clipped by its
/// own segment.
fn stacked_column(title: &str, entries: &[(&str, f64)], fmt_val: impl Fn(f64) -> String) -> String {
    let total: f64 = entries.iter().map(|(_, v)| v).sum();
    let group = |body: &str| {
        let caption = escape(title);
        format!("<div class=\"colgroup\">{body}<div class=\"collab\">{caption}</div></div>")
    };
    if total <= 0.0 {
        return group("<p class=\"nodata\">no data</p>");
    }
    let mut segments = String::new();
    for &(label, value) in entries {
        if value <= 0.0 {
            continue;
        }
        let share = value / total * 100.0;
        let inline = if share >= MIN_LABELLED_SHARE {
            format!(
                "<span class=\"seglab\" style=\"color:{}\">{share:.1}%</span>",
                verdict_ink(label),
            )
        } else {
            String::new()
        };
        let _ = write!(
            segments,
            "<div class=\"seg\" style=\"flex:{share:.4} 1 0;background:{}\" \
             title=\"{} · {} ({share:.1}%)\">{inline}</div>",
            verdict_color(label),
            escape(&verdict_name(label)),
            escape(&fmt_val(value)),
        );
    }
    group(&format!("<div class=\"col\">{segments}</div>"))
}

/// The table view of the same split: every count, volume, and share in text, so the columns are a
/// summary rather than the only way to read the numbers.
fn verdict_table(verdicts: &[VerdictStat]) -> String {
    let total_count: usize = verdicts.iter().map(|v| v.count).sum();
    let total_volume: f64 = verdicts
        .iter()
        .map(|v| v.notional_usd)
        .sum();
    let mut table = String::from(
        "<table><thead><tr><th>verdict</th><th>trades</th><th>share</th><th>volume</th>\
         <th>share</th></tr></thead><tbody>",
    );
    for verdict in verdicts {
        let _ = write!(
            table,
            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            escape(&verdict_name(&verdict.label)),
            verdict.count,
            pct(verdict.count, total_count),
            fmt_usd(verdict.notional_usd),
            share_pct(verdict.notional_usd, total_volume),
        );
    }
    table.push_str("</tbody></table>");
    table
}

fn group_section(title: &str, groups: &[GroupStats]) -> String {
    let mut table = String::from(
        "<table><thead><tr><th>name</th><th>trades</th><th>wins</th><th>losses</th>\
         <th>unsolved</th><th>win% (scored)</th><th>median bps</th><th>savings</th>\
         </tr></thead><tbody>",
    );
    for group in groups {
        let scored = group.wins + group.losses;
        let _ = write!(
            table,
            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            escape(&group.name),
            group.count,
            group.wins,
            group.losses,
            group.unsolved,
            pct(group.wins, scored),
            fmt_bps(group.median_net_bps),
            fmt_usd(group.total_improvement_usd),
        );
    }
    table.push_str("</tbody></table>");
    section(title, &table)
}

fn trades_section(title: &str, trades: &[TradeRow]) -> String {
    let mut table = String::from(
        "<table><thead><tr><th>settled tx</th><th>venue</th><th>solver</th>\
         <th>net bps</th><th>savings</th></tr></thead><tbody>",
    );
    for trade in trades {
        let _ = write!(
            table,
            "<tr><td class=\"mono\">{}</td><td>{}</td><td>{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            escape(&short_hash(&trade.settled_tx)),
            escape(&trade.venue),
            escape(&trade.solver),
            fmt_bps(trade.net_bps),
            fmt_usd(trade.improvement_usd),
        );
    }
    table.push_str("</tbody></table>");
    section(title, &table)
}

fn token_section(tokens: &[Count]) -> String {
    let mut table = String::from(
        "<table><thead><tr><th>token</th><th>appearances in unsolved</th></tr></thead><tbody>",
    );
    for token in tokens {
        let _ = write!(
            table,
            "<tr><td class=\"mono\">{}</td><td class=\"num\">{}</td></tr>",
            escape(&token.label),
            token.count,
        );
    }
    table.push_str("</tbody></table>");
    section("Unsolved token tail", &table)
}

/// The mock-`PropAMM` section: did the pool behave the way its price says it should?
///
/// Leads with one giant verdict, because a calibrated run has a right answer and the reader should
/// not have to derive it. Each group then gives its own number at a glance, coloured by outcome and
/// labelled in words — colour is never the only carrier. The narrative sits at the bottom for
/// anyone who needs to know what "fee headroom" means.
fn propamm_section(propamm: &PropAmm) -> String {
    let pair = propamm
        .pair
        .as_deref()
        .unwrap_or("unknown pair");
    let (decided, total) = propamm.conclusive();
    let (headline, cls, sub) = match propamm.verdict() {
        GroupVerdict::Pass => (
            "PASS".to_string(),
            "pos big",
            format!("{decided} of {total} price groups conclusive, none violated"),
        ),
        GroupVerdict::Fail(reason) => ("FAIL".to_string(), "neg big", reason),
        GroupVerdict::NoData => (
            "NO DATA".to_string(),
            "idlenum big",
            "no price group reached a conclusion — the run needs more orders on this pair"
                .to_string(),
        ),
    };
    let body = format!(
        "<div class=\"propammhead\">\
           <div class=\"herostat\"><div class=\"heronum {cls}\">{headline}</div>\
             <div class=\"herolab\">mock PropAMM on {}</div></div>\
           <div class=\"propammsub\">{}</div>\
         </div>{}{}{}{}\
         <p class=\"note\">Every order on this pair is priced against the route Fynd would otherwise \
          have quoted, so each one is a test rather than a measurement. <em>Fee headroom</em> is the \
          fee the signed extension could have charged and still beaten that route. Quotes from this \
          pool are not executable.</p>",
        escape(pair),
        escape(&sub),
        propamm_uplift(propamm),
        propamm_totals(propamm),
        propamm_groups(&propamm.groups),
        propamm_pair_table(&propamm.by_pair),
    );
    section("Mock PropAMM (exclusive route)", &body)
}

/// The controlled A/B: the same orders solved with the mock available and with it neutralised.
///
/// The two `with` figures are the ones to read, and the deltas beside them are what the pool
/// bought. Green means the mock helped; a negative delta would mean it cost us, which the colour
/// has to be able to say as plainly as the win.
fn propamm_uplift(propamm: &PropAmm) -> String {
    let uplift = &propamm.uplift;
    if uplift.orders == 0 {
        return "<p class=\"nodata\">No order was scored in both worlds, so there is nothing to \
                compare yet.</p>"
            .to_string();
    }
    // Takes the sign as an ordering, so a count and a float both colour the same way without either
    // being cast to the other.
    let delta = |sign: std::cmp::Ordering, formatted: &str| {
        let cls = match sign {
            std::cmp::Ordering::Greater => "pos",
            std::cmp::Ordering::Less => "neg",
            std::cmp::Ordering::Equal => "idlenum",
        };
        format!("<span class=\"delta {cls}\">{}</span>", escape(formatted))
    };
    let sign_of = |value: f64| {
        value
            .partial_cmp(&0.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    let extra_wins = uplift.extra_wins();
    let extra_profit = uplift.extra_profit_usd();
    let profit_pct = uplift
        .profit_uplift_pct()
        .map_or_else(|| "—".to_string(), |pct| format!("{pct:+.0}%"));

    let winrate_without = format!("{:.1}%", uplift.winrate_without_pct());
    let winrate_with = format!("{:.1}%", uplift.winrate_with_pct());
    let winrate_delta =
        format!("{:+.1} pts", uplift.winrate_with_pct() - uplift.winrate_without_pct());
    let wins_delta = format!("{extra_wins:+}");
    let profit_delta =
        format!("{}{}", if extra_profit >= 0.0 { "+" } else { "-" }, fmt_usd(extra_profit.abs()));

    format!(
        "<table class=\"ab\"><thead><tr><th></th><th>without PropAMM</th>\
           <th>with PropAMM</th><th>difference</th></tr></thead><tbody>\
         <tr><td>win rate</td>\
           <td class=\"num\">{}</td><td class=\"num strong\">{}</td><td class=\"num\">{}</td></tr>\
         <tr><td>orders won</td>\
           <td class=\"num\">{}</td><td class=\"num strong\">{}</td><td class=\"num\">{}</td></tr>\
         <tr><td>quoted output vs settled</td>\
           <td class=\"num\">{}</td><td class=\"num strong\">{}</td><td class=\"num\">{} ({})</td></tr>\
         <tr class=\"abfee\"><td>fee captured for LPs</td>\
           <td class=\"num\">{}</td><td class=\"num strong\">{}</td><td class=\"num\">{}</td></tr>\
         </tbody></table>\
         <p class=\"abnote\">Same {} orders, same block states — the mock's presence is the only \
          difference. The first three rows are the taker's side and barely move by design: the router \
          pins a surplus quote's output to the public reference, so the underbid is not passed on. \
          It lands in the last row instead, on {} of flow the pool took.</p>",
        winrate_without,
        winrate_with,
        delta(
            sign_of(uplift.winrate_with_pct() - uplift.winrate_without_pct()),
            &winrate_delta,
        ),
        uplift.wins_without,
        uplift.wins_with,
        delta(extra_wins.cmp(&0), &wins_delta),
        fmt_usd(uplift.profit_without_usd),
        fmt_usd(uplift.profit_with_usd),
        delta(sign_of(extra_profit), &profit_delta),
        escape(&profit_pct),
        fmt_usd(0.0),
        fmt_usd(propamm.fee_headroom_usd),
        delta(
            sign_of(propamm.fee_headroom_usd),
            &format!("+{}", fmt_usd(propamm.fee_headroom_usd)),
        ),
        uplift.orders,
        fmt_usd(propamm.captured_flow_usd),
    )
}

/// The run's totals under the verdict: how much flow the pool took and what fee it took on it.
///
/// Kept small and beneath the verdict — these are the size of the result, not the result.
fn propamm_totals(propamm: &PropAmm) -> String {
    let stat = |value: &str, label: &str| {
        format!(
            "<div class=\"ministat\"><div class=\"mininum\">{value}</div>\
             <div class=\"minilab\">{label}</div></div>"
        )
    };
    format!(
        "<div class=\"herofoot\">{}{}{}{}{}</div>",
        stat(&fmt_count(propamm.won), "orders won"),
        stat(&pct(propamm.won, propamm.solved), "of orders solved"),
        stat(&fmt_usd(propamm.captured_flow_usd), "flow captured"),
        stat(&fmt_bps(propamm.median_headroom_bps), "median fee headroom"),
        stat(&fmt_bps(propamm.avg_headroom_bps()), "fee headroom, flow-weighted"),
    )
}

/// The offset groups, one card each, ascending by price.
///
/// Each card's big number is the count the group's expectation is about: for a below-market group
/// that is how many times it was wrongly selected (so zero is the good answer), and above market it
/// is how many times it won. The number is coloured by outcome and always sits beside a word.
fn propamm_groups(groups: &[PropAmmGroup]) -> String {
    if groups.is_empty() {
        return "<p class=\"nodata\">No calibrated orders — no settled trade in this run was on \
                the mirrored pair.</p>"
            .to_string();
    }
    let mut cards = String::new();
    for group in groups {
        let (word, cls) = match &group.verdict {
            GroupVerdict::Pass => ("pass", "pos"),
            GroupVerdict::Fail(_) => ("FAIL", "neg"),
            GroupVerdict::NoData => ("no data", "idlenum"),
        };
        let detail = match &group.verdict {
            GroupVerdict::Fail(reason) => {
                format!("<p class=\"groupfail\">{}</p>", escape(reason))
            }
            GroupVerdict::Pass | GroupVerdict::NoData => String::new(),
        };
        let _ = write!(
            cards,
            "<div class=\"group\">\
               <div class=\"grouphead\"><span class=\"groupoff\">{}</span></div>\
               <div class=\"groupverdict {cls}\">{word}</div>\
               <div class=\"groupbig {cls}\">{}</div>\
               <div class=\"groupcap\">{} ({})</div>\
               <div class=\"groupfee\">fee taken <strong>{}</strong> median, {} max, \
                 of at most {}</div>\
               <div class=\"groupexp\">{}</div>{detail}\
             </div>",
            escape(&fmt_offset(group.offset_bps)),
            group.selected,
            escape(&format!("selected of {} orders", group.orders)),
            escape(&share_pct_of(group.selected_pct())),
            escape(&fmt_bps(group.median_fee_bps)),
            escape(&fmt_bps(group.max_fee_bps)),
            escape(&fmt_offset_ceiling(group.offset_bps)),
            escape(group.expectation()),
        );
    }
    format!("<div class=\"groups\">{cards}</div>")
}

/// The fee ceiling a group's price implies, for the "of at most" caption.
fn fmt_offset_ceiling(offset_bps: i32) -> String {
    if offset_bps <= 0 {
        "0.0 bps".to_string()
    } else {
        format!("{offset_bps}.0 bps")
    }
}

/// An offset as a signed bps label, so a column header reads as a price and not a bare number.
fn fmt_offset(offset_bps: i32) -> String {
    match offset_bps.cmp(&0) {
        std::cmp::Ordering::Less => format!("{offset_bps} bps (below market)"),
        std::cmp::Ordering::Equal => "at market".to_string(),
        std::cmp::Ordering::Greater => format!("+{offset_bps} bps (above market)"),
    }
}

/// A percentage that is already computed, rendered like the report's other shares.
fn share_pct_of(pct: f64) -> String {
    format!("{pct:.0}%")
}

/// Per-order-direction breakdown. A row's pair can differ from the mirrored pair: the mock serves
/// any route with a leg it can price, so a `DAI→USDC` order routed through `WETH` shows up here
/// too.
fn propamm_pair_table(pairs: &[PropAmmPair]) -> String {
    if pairs.is_empty() {
        return "<p class=\"nodata\">no orders</p>".to_string();
    }
    let mut table = String::from(
        "<table><thead><tr><th>order pair</th><th>solved</th><th>wins</th><th>win%</th>\
         <th>flow captured</th><th>fee headroom</th><th>median headroom bps</th>\
         </tr></thead><tbody>",
    );
    for pair in pairs {
        let _ = write!(
            table,
            "<tr><td class=\"mono\" title=\"{} → {}\">{} → {}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            escape(&pair.token_in),
            escape(&pair.token_out),
            escape(&short_hash(&pair.token_in)),
            escape(&short_hash(&pair.token_out)),
            pair.solved,
            pair.won,
            pct(pair.won, pair.solved),
            fmt_usd(pair.captured_flow_usd),
            fmt_usd(pair.fee_headroom_usd),
            fmt_bps(pair.median_headroom_bps),
        );
    }
    table.push_str("</tbody></table>");
    table
}

fn section(title: &str, body: &str) -> String {
    format!("<section><h2>{}</h2>{body}</section>", escape(title))
}

/// `part / whole` as `f64`, `0.0` when `whole` is zero.
#[expect(clippy::cast_precision_loss, reason = "trade counts are far below f64's mantissa")]
fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    part as f64 / whole as f64
}

/// `count` as a percentage of `whole`, one decimal; em dash when `whole` is zero.
fn pct(count: usize, whole: usize) -> String {
    if whole == 0 {
        return "—".to_string();
    }
    format!("{:.1}%", ratio(count, whole) * 100.0)
}

/// `part` as a percentage of a USD `whole`, one decimal; em dash when there is no volume to divide.
fn share_pct(part: f64, whole: f64) -> String {
    if whole <= 0.0 {
        return "—".to_string();
    }
    format!("{:.1}%", part / whole * 100.0)
}

fn fmt_bps(bps: Option<f64>) -> String {
    bps.map_or_else(|| "—".to_string(), |b| format!("{b:.1}"))
}

/// Basis points with an explicit sign, for a headline stat (e.g. `+12.3`); em dash when absent.
fn fmt_bps_signed(bps: Option<f64>) -> String {
    bps.map_or_else(|| "—".to_string(), |b| format!("{b:+.1}"))
}

/// A signed USD amount with a thousands-separated integer part, e.g. `-$1,234.56`. A non-finite
/// amount — an overflowed sum, or a `null` the aggregates turned into a NaN — has no digits to
/// group and renders as an em dash.
fn fmt_usd(value: f64) -> String {
    if !value.is_finite() {
        return "—".to_string();
    }
    let sign = if value < 0.0 { "-$" } else { "$" };
    let rounded = format!("{:.2}", value.abs());
    // `{:.2}` on a finite value always emits `<digits>.<2 digits>`, so the last three bytes are the
    // fraction and everything before them is the integer part. Both are ASCII.
    let (int_part, frac) = rounded.split_at(rounded.len() - 3);
    format!("{sign}{}{frac}", group_thousands(int_part))
}

/// A count with thousands separators, e.g. `4,433`.
fn fmt_count(count: usize) -> String {
    group_thousands(&count.to_string())
}

fn group_thousands(digits: &str) -> String {
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Shorten a hex hash to `0x1234abcd…9abc` for a table cell.
fn short_hash(hash: &str) -> String {
    if hash.len() <= 18 {
        return hash.to_string();
    }
    format!("{}…{}", &hash[..10], &hash[hash.len() - 6..])
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const HEAD: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>hindsight report</title>
<style>
:root { color-scheme: dark; }
body { margin: 0; padding: 2rem; background: #17111f; color: #ece7f2;
  font: 14px/1.5 -apple-system, Segoe UI, Roboto, sans-serif; }
h2 { margin: 0 0 1rem; font-size: 1.15rem; color: #c9b6ef; }
h3 { margin: 0 0 .75rem; font-size: .9rem; color: #9a8bbf; text-transform: uppercase; letter-spacing: .04em; }
section { background: #211a30; border: 1px solid #362b4a; border-radius: 8px;
  padding: 1.25rem 1.5rem; margin-bottom: 1.25rem; }
.hero { }
.heroscope { color: #9a8bbf; font-size: .85rem; margin-bottom: 1rem; }
.chip { display: inline-block; padding: .1rem .55rem; border-radius: 999px; background: #17111f;
  border: 1px solid #362b4a; margin-left: .35rem; font-size: .8rem; }
.chip.on { color: #c9b6ef; border-color: #5a4680; }
.herostats { display: flex; flex-wrap: wrap; gap: 1.5rem; }
.herostat { flex: 1 1 0; min-width: 11rem; }
.heronum { font-size: 4.5rem; font-weight: 700; line-height: 1.05; letter-spacing: -0.02em; }
.heronum.big { font-size: 5.5rem; }
.heronum.pos { color: #66bb6a; }
.heronum.neg { color: #ef5350; }
.herolab { color: #9a8bbf; font-size: .8rem; text-transform: uppercase; letter-spacing: .04em; margin-top: .3rem; }
.herofoot { display: flex; flex-wrap: wrap; gap: 2.5rem; margin-top: 1.75rem; }
.mininum { font-size: 1.4rem; font-weight: 600; line-height: 1.2; }
.minilab { color: #9a8bbf; font-size: .75rem; text-transform: uppercase; letter-spacing: .04em; }
.legend { list-style: none; margin: 0; padding: 0; }
.legend li { display: flex; align-items: center; gap: .6rem; padding: .25rem 0; }
.legend.row { display: flex; flex-wrap: wrap; gap: 1.25rem; margin-bottom: 1.25rem; }
.legend.row li { padding: 0; }
.swatch { width: .85rem; height: .85rem; border-radius: 2px; flex: 0 0 auto; }
.legname { text-transform: capitalize; }
/* The columns are the panel's focus: centred, and wide enough to read at a glance. The table below
   restates their values. */
.cols { display: flex; justify-content: center; gap: 2rem; margin: .5rem 0 2rem; }
.colgroup { display: flex; flex-direction: column; gap: .6rem; flex: 0 1 15rem; }
/* Segments stack top down in verdict order, so the win green caps the column. A 2px gap in the
   surface colour separates them — never a border. */
.col { display: flex; flex-direction: column; gap: 2px; height: 17rem; }
.seg { min-height: 2px; display: flex; align-items: center; justify-content: center; }
.seg:first-child { border-radius: 4px 4px 0 0; }
.seg:last-child { border-radius: 0 0 4px 4px; }
.seglab { font-size: .8rem; font-weight: 600; font-variant-numeric: tabular-nums; }
.collab { color: #9a8bbf; font-size: .75rem; text-transform: uppercase; letter-spacing: .04em;
  text-align: center; }
.nodata { color: #9a8bbf; margin: 0; }
/* A section's framing sentence: what the numbers below mean, before they are read. */
.note { color: #b9adcf; margin: 0 0 .5rem; max-width: 68ch; line-height: 1.55; }
/* The run's verdict, sized like the report's headline savings figure. */
.propammhead { display: flex; flex-wrap: wrap; align-items: baseline; gap: 2rem; }
.propammsub { color: #b9adcf; font-size: .9rem; max-width: 52ch; line-height: 1.5; }
.idlenum { color: #9a8bbf; }
/* One card per offset group. They wrap rather than scroll, so a wider ladder stays readable. */
.groups { display: flex; flex-wrap: wrap; gap: 1rem; margin: 1.5rem 0; }
.group { flex: 1 1 15rem; border: 1px solid #2f2540; border-radius: 6px; padding: .9rem 1rem; }
.grouphead { display: flex; align-items: center; justify-content: space-between; gap: .75rem; }
.groupoff { font-weight: 600; font-size: .9rem; }
/* The number the group's expectation is about, big enough to read across a room. */
.groupbig { font-size: 2.6rem; font-weight: 700; line-height: 1.05; margin-top: .5rem; }
.groupcap { color: #9a8bbf; font-size: .78rem; }
.groupfee { color: #b9adcf; font-size: .82rem; margin-top: .6rem; }
.groupexp { color: #6f6390; font-size: .72rem; margin-top: .5rem; }
/* The verdict never rests on colour alone: each chip is also labelled pass / FAIL / no data. */
.chip.poschip { background: #1b3a1f; color: #a5d6a7; }
.chip.negchip { background: #4a1c1c; color: #ef9a9a; }
.chip.idlenumchip { background: #2f2540; color: #9a8bbf; }
.groupfail { color: #ef9a9a; font-size: .78rem; margin: .6rem 0 0; line-height: 1.45; }
/* Each sub-test carries its own verdict word, sized so the three read at a glance together. */
.groupverdict { font-size: 1.5rem; font-weight: 700; letter-spacing: .04em; margin-top: .45rem; }
/* The with/without table: the "with" column is the answer, so it carries the weight. */
.ab { margin: 1.5rem 0 .25rem; }
.ab th { color: #9a8bbf; font-weight: 500; }
.ab td.strong { font-weight: 700; font-size: 1.05rem; }
.delta { font-weight: 600; }
.abnote { color: #6f6390; font-size: .76rem; margin: 0 0 .5rem; max-width: 74ch; line-height: 1.5; }
/* Where the underbid actually lands, set apart from the taker-side rows above it. */
.ab tr.abfee td { border-top: 1px solid #2f2540; padding-top: .55rem; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: .4rem .6rem; border-bottom: 1px solid #362b4a; }
th { color: #9a8bbf; font-weight: 600; font-size: .8rem; text-transform: uppercase; }
td.num { text-align: right; font-variant-numeric: tabular-nums; }
td.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
</style></head><body>
"#;

const FOOT: &str = "</body></html>\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{
        aggregate::{build, Report},
        record::Comparison,
    };

    fn sample_report() -> Report {
        let records: Vec<Comparison> = vec![
            serde_json::json!({
                "block": 1, "settled_tx": "0xabc0000000000000000000000000000000000000000000000000000000000001",
                "venue": "relay", "solver": "1inch", "token_in": "0xaaa", "token_out": "0xbbb",
                "top": {"verdict": "win", "net_bps": 20.0, "improvement_usd": 12.0, "settled_value_usd": 1000.0}
            }),
            serde_json::json!({
                "block": 2, "settled_tx": "0xdef0000000000000000000000000000000000000000000000000000000000002",
                "venue": "relay", "solver": "0x", "token_in": "0xccc", "token_out": "0xddd",
                "top": {"verdict": "unsolvable", "net_bps": null, "improvement_usd": null, "settled_value_usd": 50.0}
            }),
        ]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        build(&records)
    }

    #[test]
    fn test_render_is_self_contained_and_covers_sections() {
        let html = render(&sample_report(), None);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        // No external assets: the file must open offline.
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("src="));
        for heading in ["Fynd savings", "Verdicts", "Top savings", "By solver"] {
            assert!(html.contains(heading), "missing content: {heading}");
        }
        // Verdicts render as two stacked columns — by trade count and by volume — not pies, and
        // not a separate coverage section.
        assert!(!html.contains("conic-gradient"));
        assert!(html.contains("class=\"col\""));
        assert!(html.contains("by trade count") && html.contains("by volume"));
        assert!(!html.contains(">Coverage<"));
    }

    #[test]
    fn test_stacked_column_labels_only_segments_tall_enough_to_hold_one() {
        let entries = [("win", 95.0), ("loss", 5.0)];
        let html = stacked_column("by trade count", &entries, |v| format!("{v:.0}"));
        // The 95% segment carries its share inline; the 5% one would clip it, so its value lives
        // in the hover title instead.
        assert!(html.contains(">95.0%<"), "{html}");
        assert!(!html.contains(">5.0%<"), "{html}");
        assert!(html.contains("title=\"loss · 5 (5.0%)\""), "{html}");
        // Segments grow proportionally and are separated by the surface gap, not a border.
        assert!(html.contains("flex:95.0000 1 0"), "{html}");
        assert!(!html.contains("border"), "{html}");
    }

    #[test]
    fn test_stacked_column_stacks_the_first_verdict_at_the_top() {
        let entries = [("win", 50.0), ("loss", 50.0)];
        let html = stacked_column("by trade count", &entries, |v| format!("{v:.0}"));
        // Plain `column` renders markup order top down, so the win green caps the column.
        let win = html
            .find("title=\"win")
            .expect("win segment");
        let loss = html
            .find("title=\"loss")
            .expect("loss segment");
        assert!(win < loss, "{html}");
    }

    #[test]
    fn test_stacked_column_without_volume_says_so() {
        let html = stacked_column("by volume (settled USD)", &[("win", 0.0)], fmt_usd);
        assert!(html.contains("no data"));
        assert!(!html.contains("class=\"col\""));
        // The caption still says which view is empty.
        assert!(html.contains("by volume (settled USD)"));
    }

    #[test]
    fn test_verdict_table_states_every_share() {
        let html = render(&sample_report(), None);
        // Two trades, one of each verdict: 50% by count, and the win holds 1000 of 1050 volume.
        assert!(html.contains("<td>win</td>"), "{html}");
        assert!(html.contains("95.2%"), "{html}");
        assert!(html.contains("50.0%"), "{html}");
    }

    #[test]
    fn test_render_shows_savings_and_top_trades() {
        let html = render(&sample_report(), None);
        // Net savings headline and the win in the top-savings table.
        assert!(html.contains("$12.00"));
        assert!(html.contains("0xabc00000…000001")); // shortened hash
        assert!(html.contains("all venues"));
    }

    #[test]
    fn test_render_states_the_run_size_as_counts() {
        let html = render(&sample_report(), None);
        // Two records over two blocks, one of them scored — plain counts, no sentence.
        for (value, label) in [("2", "blocks"), ("2", "comparisons"), ("1", "scored")] {
            assert!(
                html.contains(&format!(">{value}</div><div class=\"minilab\">{label}<")),
                "{label}"
            );
        }
        // The prose summary of the loss drag and net is gone.
        assert!(!html.contains("given up on"), "{html}");
        assert!(!html.contains("herosub"), "{html}");
    }

    #[test]
    fn test_render_names_the_active_venue_filter() {
        let html = render(&sample_report(), Some("relay"));
        assert!(html.contains("venue: relay"));
        assert!(!html.contains("all venues"));
    }

    #[test]
    fn test_fmt_usd_groups_and_signs() {
        assert_eq!(fmt_usd(1_234_567.5), "$1,234,567.50");
        assert_eq!(fmt_usd(-260.03), "-$260.03");
        assert_eq!(fmt_usd(0.0), "$0.00");
    }

    #[test]
    fn test_fmt_usd_non_finite() {
        // `{:.2}` renders these without a decimal point, so they have no integer part to group.
        assert_eq!(fmt_usd(f64::INFINITY), "—");
        assert_eq!(fmt_usd(f64::NEG_INFINITY), "—");
        assert_eq!(fmt_usd(f64::NAN), "—");
    }

    #[test]
    fn test_escape_replaces_markup() {
        assert_eq!(escape("<a>&\"x\""), "&lt;a&gt;&amp;&quot;x&quot;");
    }

    /// A report from records carrying mock-`PropAMM` outcomes.
    fn propamm_report(won: bool) -> Report {
        let records: Vec<Comparison> = vec![serde_json::json!({
            "block": 1,
            "settled_tx": "0xabc0000000000000000000000000000000000000000000000000000000000001",
            "venue": "relay", "solver": "1inch",
            "token_in": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "token_out": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "top": {"verdict": "win", "net_bps": 20.0, "improvement_usd": 12.0, "settled_value_usd": 1000.0},
            "propamm": {
                "pair": "WETH/USDC", "won": won,
                "fee_headroom_bps": won.then_some(4.0),
                "committed_usd": won.then_some(1_000.0),
                "fee_headroom_usd": won.then_some(0.4),
            },
        })]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        build(&records)
    }

    #[test]
    fn test_propamm_section_omitted_without_the_harness() {
        // An ordinary run's report must look exactly as it did before the harness existed.
        assert!(!render(&sample_report(), None).contains("Mock PropAMM"));
    }

    #[test]
    fn test_propamm_section_names_the_mirrored_pair_and_its_numbers() {
        let html = render(&propamm_report(true), None);
        assert!(html.contains("Mock PropAMM (exclusive route)"));
        assert!(html.contains("WETH/USDC"), "the section must name the pool it stood in for");
        assert!(html.contains("median fee headroom"));
        // The order pair is rendered short, with the full addresses in the hover title.
        assert!(html.contains("0xc02aaa39…756cc2"));
        assert!(html.contains("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"));
    }

    #[test]
    fn test_propamm_section_renders_a_run_the_pool_never_won() {
        // A zero-win run is a result, not an error: the section still renders, with an em dash
        // where the median headroom is undefined.
        let html = render(&propamm_report(false), None);
        assert!(html.contains("Mock PropAMM (exclusive route)"));
        assert!(html.contains("0.0%"), "a zero winrate must be stated, not omitted");
    }

    /// A report from calibrated records across the three offset groups.
    fn group_report(above_fee_bps: f64) -> Report {
        let record = |block: u64, offset: i32, won: bool, fee: f64| {
            serde_json::json!({
                "block": block,
                "settled_tx": format!("0x{block:064x}"),
                "venue": "relay", "solver": "1inch",
                "token_in": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "token_out": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                "top": {"verdict": "win", "net_bps": 5.0, "settled_value_usd": 1000.0},
                "propamm": {
                    "pair": "WETH/USDT", "offset_bps": offset, "won": won,
                    "fee_headroom_bps": won.then_some(fee),
                    "committed_usd": won.then_some(1_000.0),
                    "fee_headroom_usd": won.then_some(fee / 10.0),
                    "without_won": false, "with_won": won,
                    "without_improvement_usd": -1.0,
                    "with_improvement_usd": if won { 4.0 } else { -1.0 },
                },
            })
        };
        let records: Vec<Comparison> = vec![
            record(1, -5, false, 0.0),
            record(2, 0, true, 0.0),
            record(3, 5, true, above_fee_bps),
        ]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        build(&records)
    }

    #[test]
    fn test_propamm_section_leads_the_document() {
        // A calibrated run exists to answer one question, so its verdict comes before the savings
        // headline rather than three sections down.
        let html = render(&group_report(5.0), None);
        let propamm = html
            .find("Mock PropAMM")
            .expect("section present");
        let savings = html
            .find("Fynd savings")
            .expect("hero present");
        assert!(propamm < savings, "the PropAMM verdict must precede the savings headline");
    }

    #[test]
    fn test_overall_verdict_is_a_single_word() {
        assert!(render(&group_report(5.0), None).contains(">PASS<"));
        assert!(render(&group_report(20.0), None).contains(">FAIL<"));
    }

    #[test]
    fn test_group_cards_lead_with_the_verdict_and_name_each_price() {
        let html = render(&group_report(5.0), None);
        assert!(html.contains("-5 bps (below market)"));
        assert!(html.contains("at market"));
        assert!(html.contains("+5 bps (above market)"));
        // The verdict is a word, never colour alone.
        assert!(html.contains(">pass<"), "a passing group must say so in text");
        assert!(html.contains("never selected"));
        assert!(html.contains("selected only on gas, zero fee"));
    }

    #[test]
    fn test_a_failing_group_renders_the_reason_not_just_a_colour() {
        let html = render(&group_report(20.0), None);
        assert!(html.contains(">FAIL<"));
        assert!(
            html.contains("took a fee of 20.00 bps"),
            "the reason must be readable without opening the JSONL"
        );
    }

    #[test]
    fn test_group_panel_says_so_when_nothing_was_calibrated() {
        // A run whose settled trades never touched the mirrored pair must explain the empty panel
        // rather than render three blank cards.
        let html = render(&propamm_report(true), None);
        assert!(html.contains("No calibrated orders"));
    }

    #[test]
    fn test_uplift_table_shows_both_worlds_and_the_difference() {
        let html = render(&group_report(5.0), None);
        assert!(html.contains("without PropAMM"));
        assert!(html.contains("with PropAMM"));
        assert!(html.contains("win rate"));
        assert!(html.contains("quoted output vs settled"));
        assert!(html.contains("fee captured for LPs"));
        // The difference must be signed, so a regression reads as one.
        assert!(html.contains("delta"), "each row carries a signed difference");
    }

    #[test]
    fn test_each_group_carries_its_own_verdict_word() {
        // Three sub-tests, three verdicts — the overall PASS must not be the only one visible.
        let html = render(&group_report(5.0), None);
        let verdicts = html
            .matches("class=\"groupverdict")
            .count();
        assert_eq!(verdicts, 3, "one verdict per offset group");
    }

    #[test]
    fn test_uplift_table_says_so_when_nothing_was_scored_in_both_worlds() {
        let html = render(&propamm_report(true), None);
        assert!(html.contains("nothing to"), "an empty A/B must explain itself");
    }
}
