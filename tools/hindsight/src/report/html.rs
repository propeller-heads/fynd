//! Render an aggregated [`Report`] to a single self-contained HTML file.
//!
//! No external assets or network: the verdict split is a flexbox stacked column, styling is one
//! inline stylesheet, so the file opens offline. The panels mirror the dashboard's value views —
//! the headline Fynd savings (`hindsight_savings_usd`), the verdict split, coverage by notional,
//! per-solver/venue breakdowns, and the top-saving trades — and skip the block/latency health
//! panels the JSONL does not carry.

use std::fmt::Write as _;

use crate::report::aggregate::{
    Count, GroupStats, GroupVerdict, PropAmm, PropAmmGroup, Report, Savings, Summary, TradeRow,
    VerdictStat,
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
    // A calibrated run's headline is the with/without split, so it replaces the single-world hero
    // rather than sitting beside a number that silently mixes the two.
    match report.propamm.as_ref() {
        Some(propamm) => html.push_str(&propamm_hero(propamm, &report.summary, filter)),
        None => html.push_str(&hero_section(&report.savings, &report.summary, filter)),
    }
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

/// The headline for a calibrated run: the same orders scored with and without the mock, side by
/// side, plus one verdict per price group.
///
/// Two columns of the same three figures the single-world hero shows, so the comparison is read by
/// scanning across rather than by reading a caption. Everything is a number: the deltas carry the
/// sign, colour carries the direction, and each figure is labelled.
fn propamm_hero(propamm: &PropAmm, summary: &Summary, filter: Option<&str>) -> String {
    let uplift = &propamm.uplift;
    let scope = filter.map_or_else(
        || "<span class=\"chip\">all venues</span>".to_string(),
        |venue| format!("<span class=\"chip on\">venue: {}</span>", escape(venue)),
    );
    let pair = propamm
        .pair
        .as_deref()
        .unwrap_or("unknown pair");
    let (verdict, verdict_cls) = match propamm.verdict() {
        GroupVerdict::Pass => ("PASS", "pos"),
        GroupVerdict::Fail(_) => ("FAIL", "neg"),
        GroupVerdict::NoData => ("NO DATA", "idlenum"),
    };
    let mini = |value: &str, label: &str| {
        format!(
            "<div class=\"ministat\"><div class=\"mininum\">{value}</div>\
             <div class=\"minilab\">{label}</div></div>"
        )
    };
    format!(
        "<section class=\"hero\">\
           <div class=\"heroscope\">hindsight report {scope}\
             <span class=\"chip {verdict_cls}chip\">mock PropAMM {verdict}</span>\
             <span class=\"chip\">{}</span></div>\
           {}\
           <div class=\"worlds\">{}{}</div>\
           <div class=\"lprow\">{}</div>\
           <div class=\"herofoot\">{}{}{}{}{}</div>\
         </section>",
        escape(pair),
        propamm_tests(propamm),
        world_column(
            "without PropAMM",
            "",
            uplift.profit_without_usd,
            uplift.winrate_without_pct(),
            uplift.median_bps_without,
            None
        ),
        world_column(
            "with PropAMM",
            "on",
            uplift.profit_with_usd,
            uplift.winrate_with_pct(),
            uplift.median_bps_with,
            Some((
                uplift.extra_profit_usd(),
                uplift.winrate_with_pct() - uplift.winrate_without_pct()
            ))
        ),
        lp_capture(propamm),
        mini(&fmt_count(summary.distinct_blocks), "blocks"),
        mini(&fmt_count(uplift.orders), "orders scored both ways"),
        mini(&format!("{:+}", uplift.extra_wins()), "extra orders won"),
        mini(&fmt_count(summary.total), "comparisons"),
        mini(&fmt_usd(propamm.captured_flow_usd), "flow through the pool"),
    )
}

/// The three price tests: one banner verdict, then one card per test.
///
/// Each card leads with what it is testing — the pool's price against the best public route — then
/// states the rule and what happened, so a reader never has to translate a basis-point offset into
/// an expectation themselves.
fn propamm_tests(propamm: &PropAmm) -> String {
    let (decided, total) = propamm.conclusive();
    let (banner, cls) = match propamm.verdict() {
        GroupVerdict::Pass => ("TESTS PASSED", "pos"),
        GroupVerdict::Fail(_) => ("TESTS FAILED", "neg"),
        GroupVerdict::NoData => ("NO CONCLUSION YET", "idlenum"),
    };
    format!(
        "<div class=\"testsbanner\">\
           <div class=\"heronum {cls} big\">{banner}</div>\
           <div class=\"herolab\">{decided} of {total} price tests conclusive</div>\
         </div>{}",
        propamm_groups(&propamm.groups),
    )
}

/// One world's three headline figures, with the deltas attached to the second column.
fn world_column(
    title: &str,
    modifier: &str,
    profit_usd: f64,
    winrate_pct: f64,
    median_bps: Option<f64>,
    deltas: Option<(f64, f64)>,
) -> String {
    let (profit_delta, winrate_delta) = deltas.map_or_else(
        || (String::new(), String::new()),
        |(profit, winrate)| {
            (
                signed_delta(profit, &fmt_usd_signed(profit)),
                signed_delta(winrate, &format!("{winrate:+.1} pts")),
            )
        },
    );
    let big = |value: &str, label: &str, delta: &str| {
        format!(
            "<div class=\"herostat\"><div class=\"heronum pos big\">{value}</div>\
             <div class=\"herolab\">{label} {delta}</div></div>"
        )
    };
    format!(
        "<div class=\"world {modifier}\"><div class=\"worldtitle\">{}</div>{}{}{}</div>",
        escape(title),
        big(&fmt_usd(profit_usd), "Fynd savings (wins uplift)", &profit_delta),
        big(&format!("{winrate_pct:.1}%"), "win rate", &winrate_delta),
        big(&fmt_bps_signed(median_bps), "median savings bps (wins)", ""),
    )
}

/// The fee the pool captured for its LPs — where the underbid lands, since the taker's quote is
/// pinned to the public reference.
fn lp_capture(propamm: &PropAmm) -> String {
    format!(
        "<div class=\"herostat\"><div class=\"heronum pos big\">+{}</div>\
         <div class=\"herolab\">captured for LPs {}</div></div>",
        fmt_usd(propamm.fee_headroom_usd),
        signed_delta(1.0, &fmt_bps(propamm.avg_headroom_bps())),
    )
}

/// A signed figure coloured by direction. Colour is never the only carrier — the sign is in the
/// text.
fn signed_delta(value: f64, formatted: &str) -> String {
    let cls = match value
        .partial_cmp(&0.0)
        .unwrap_or(std::cmp::Ordering::Equal)
    {
        std::cmp::Ordering::Greater => "pos",
        std::cmp::Ordering::Less => "neg",
        std::cmp::Ordering::Equal => "idlenum",
    };
    format!("<span class=\"delta {cls}\">{}</span>", escape(formatted))
}

/// A USD amount with an explicit sign, for a delta.
fn fmt_usd_signed(value: f64) -> String {
    format!("{}{}", if value < 0.0 { "-" } else { "+" }, fmt_usd(value.abs()))
}

/// One card per price test, ascending by price.
///
/// The verdict word is the largest thing on the card, because that is the answer. Under it the rule
/// and the observation sit as two plain sentences, so "pass" is always accompanied by what passed.
fn propamm_groups(groups: &[PropAmmGroup]) -> String {
    if groups.is_empty() {
        return "<p class=\"nodata\">No calibrated orders yet — no settled trade so far was on the \
                mirrored pair.</p>"
            .to_string();
    }
    let mut cards = String::new();
    for (index, group) in groups.iter().enumerate() {
        let (word, cls) = match &group.verdict {
            GroupVerdict::Pass => ("PASS", "pos"),
            GroupVerdict::Fail(_) => ("FAIL", "neg"),
            GroupVerdict::NoData => ("NO DATA", "idlenum"),
        };
        let detail = match &group.verdict {
            GroupVerdict::Fail(reason) => {
                format!("<p class=\"groupfail\">{}</p>", escape(reason))
            }
            GroupVerdict::Pass | GroupVerdict::NoData => String::new(),
        };
        let _ = write!(
            cards,
            "<div class=\"group {cls}card\">\
               <div class=\"grouphead\">{}. {}</div>\
               <div class=\"groupoff\">{}</div>\
               <div class=\"groupverdict {cls}\">{word}</div>\
               <div class=\"grouprule\">{}</div>\
               <div class=\"groupsaw\">{}</div>{detail}\
             </div>",
            index + 1,
            escape(group.title()),
            escape(&fmt_offset(group.offset_bps)),
            escape(&group.expectation()),
            escape(&group.outcome()),
        );
    }
    format!("<div class=\"groups\">{cards}</div>")
}

/// An offset as a signed bps label, so a column header reads as a price and not a bare number.
fn fmt_offset(offset_bps: i32) -> String {
    match offset_bps.cmp(&0) {
        std::cmp::Ordering::Less => format!("set {} bps below it", -offset_bps),
        std::cmp::Ordering::Equal => "set exactly on it".to_string(),
        std::cmp::Ordering::Greater => format!("set {offset_bps} bps above it"),
    }
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
.idlenum { color: #9a8bbf; }
/* The two worlds side by side, so the comparison is read by scanning across. */
.worlds { display: flex; flex-wrap: wrap; gap: 1.5rem; margin-top: 1.5rem; }
.world { flex: 1 1 20rem; border: 1px solid #2f2540; border-radius: 8px; padding: 1.1rem 1.3rem; }
/* The "with" column is the answer, so it is the one that is lit. */
.world.on { border-color: #43a047; background: #16241a; }
.worldtitle { color: #9a8bbf; font-size: .78rem; text-transform: uppercase; letter-spacing: .06em;
              margin-bottom: .75rem; }
.world .herostat { margin-bottom: .9rem; }
.world .herostat:last-child { margin-bottom: 0; }
.lprow { margin-top: 1.5rem; }
/* One card per offset group. They wrap rather than scroll, so a wider ladder stays readable. */
.groups { display: flex; flex-wrap: wrap; gap: 1rem; margin: 1.5rem 0; }
.group { flex: 1 1 15rem; border: 1px solid #2f2540; border-radius: 6px; padding: .9rem 1rem; }
.grouphead { font-weight: 600; font-size: .95rem; }
.groupoff { color: #9a8bbf; font-size: .78rem; margin-top: .15rem; }
.grouprule { color: #b9adcf; font-size: .82rem; line-height: 1.45; }
.groupsaw { color: #e9e2f5; font-size: .82rem; line-height: 1.45; margin-top: .35rem; font-weight: 600; }
/* A card's border echoes its verdict, but the verdict word above it is what carries the meaning. */
.group.poscard { border-color: #43a047; background: #16241a; }
.group.negcard { border-color: #e53935; background: #241616; }
/* The tests banner: the single answer, above the three cards that justify it. */
.testsbanner { margin-top: 1.75rem; }
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
.groupverdict { font-size: 2.4rem; font-weight: 800; letter-spacing: .04em; margin: .5rem 0 .6rem; }
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
    /// A report from calibrated records across the three offset groups, with the A/B attached.
    fn group_report(above_fee_bps: f64) -> Report {
        let record = |block: u64, offset: i32, won: bool, fee: f64| {
            serde_json::json!({
                "block": block,
                "settled_tx": format!("0x{block:064x}"),
                "venue": "relay", "solver": "1inch",
                "token_in": "0x0000000000000000000000000000000000000000",
                "token_out": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "top": {"verdict": "win", "net_bps": 5.0, "improvement_usd": 4.0,
                        "settled_value_usd": 1000.0},
                "propamm": {
                    "pair": "ETH/USDC", "offset_bps": offset, "won": won,
                    "fee_headroom_bps": won.then_some(fee),
                    "committed_usd": won.then_some(1_000.0),
                    "fee_headroom_usd": won.then_some(fee / 10.0),
                    "without_won": true, "with_won": true,
                    "without_improvement_usd": 4.0, "with_improvement_usd": 4.0,
                    "without_net_bps": 5.0, "with_net_bps": 5.0,
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

    /// A calibrated run whose records carry no offset, so no group reaches a conclusion.
    fn uncalibrated_report() -> Report {
        let records: Vec<Comparison> = vec![serde_json::json!({
            "block": 1,
            "settled_tx": "0xabc0000000000000000000000000000000000000000000000000000000000001",
            "venue": "relay", "solver": "1inch",
            "token_in": "0xaaa", "token_out": "0xbbb",
            "top": {"verdict": "win", "net_bps": 20.0, "improvement_usd": 12.0,
                    "settled_value_usd": 1000.0},
            "propamm": {"pair": "ETH/USDC", "won": false},
        })]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        build(&records)
    }

    #[test]
    fn test_ordinary_run_keeps_the_single_world_hero() {
        // A run without the harness must look exactly as it did before it existed.
        let html = render(&sample_report(), None);
        assert!(html.contains("Fynd savings"));
        assert!(!html.contains("without PropAMM"));
        assert!(!html.contains("mock PropAMM"));
    }

    #[test]
    fn test_calibrated_run_splits_the_hero_into_both_worlds() {
        // The point of the redesign: the headline figures appear twice, once per world, rather than
        // one number that silently mixes them.
        let html = render(&group_report(5.0), None);
        assert!(html.contains("without PropAMM"));
        assert!(html.contains("with PropAMM"));
        assert_eq!(
            html.matches("Fynd savings (wins uplift)")
                .count(),
            2,
            "the savings figure is stated for each world"
        );
        assert_eq!(
            html.matches("herolab\">win rate")
                .count(),
            2
        );
        assert_eq!(
            html.matches("median savings bps (wins)")
                .count(),
            2
        );
    }

    #[test]
    fn test_split_hero_replaces_rather_than_joins_the_single_world_one() {
        // Two heroes would mean two different win rates on one page, one of them ambiguous.
        let html = render(&group_report(5.0), None);
        assert_eq!(html.matches("class=\"hero\"").count(), 1);
    }

    #[test]
    fn test_hero_names_the_pair_and_the_overall_verdict() {
        let html = render(&group_report(5.0), None);
        assert!(html.contains("ETH/USDC"), "the pair is named by symbol, not by address");
        assert!(html.contains("mock PropAMM PASS"));
        assert!(render(&group_report(20.0), None).contains("mock PropAMM FAIL"));
    }

    #[test]
    fn test_hero_shows_where_the_underbid_lands() {
        // The taker-side figures barely move by design, so the LP capture has to be on the page or
        // the run reads as a null result.
        let html = render(&group_report(5.0), None);
        assert!(html.contains("captured for LPs"));
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
    fn test_group_cards_name_each_test_in_terms_of_the_best_route() {
        // A reader should not have to turn a basis-point offset into an expectation themselves.
        let html = render(&group_report(5.0), None);
        assert!(html.contains("Priced worse than the best route"));
        assert!(html.contains("Priced equal to the best route"));
        assert!(html.contains("Priced better than the best route"));
        assert!(html.contains("Must never be chosen."));
        assert!(html.contains("charges no fee"));
        assert!(html.contains("cannot charge more than the 5 bps gap"));
        // And each card says what actually happened next to what should have.
        assert!(html.contains("Never chosen, across"));
    }

    #[test]
    fn test_tests_banner_states_one_answer_for_the_three_cards() {
        assert!(render(&group_report(5.0), None).contains("TESTS PASSED"));
        assert!(render(&group_report(20.0), None).contains("TESTS FAILED"));
        assert!(render(&group_report(5.0), None).contains("price tests conclusive"));
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
        assert!(render(&uncalibrated_report(), None).contains("No calibrated orders"));
    }
}
