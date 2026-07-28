//! Render an aggregated [`Report`] to a single self-contained HTML file.
//!
//! No external assets or network: the verdict split is a flexbox stacked column, styling is one
//! inline stylesheet, so the file opens offline. The panels mirror the dashboard's value views —
//! the headline Fynd savings (`hindsight_savings_usd`), the verdict split, coverage by notional,
//! per-solver/venue breakdowns, and the top-saving trades — and skip the block/latency health
//! panels the JSONL does not carry.

use std::fmt::Write as _;

use crate::report::aggregate::{
    Count, GroupStats, PropAmm, PropAmmPair, Report, Savings, Summary, TradeRow, VerdictStat,
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
    html.push_str(&hero_section(&report.savings, &report.summary, filter));
    html.push_str(&verdict_section(&report.verdicts));
    if let Some(propamm) = report.propamm.as_ref() {
        html.push_str(&propamm_section(propamm));
    }
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

/// The mock-`PropAMM` section: what the exclusive route won, and what fee it could have charged on
/// top of the flow it took.
///
/// Rendered only for a run driven with `monitor --propamm-pair`. The headline winrate is over every
/// solved order in the run, most of which never touch the mirrored pair — so it reads low by
/// construction, and the per-pair table below is where the real answer is. Both are shown rather
/// than only the flattering one.
fn propamm_section(propamm: &PropAmm) -> String {
    let pair = propamm
        .pair
        .as_deref()
        .unwrap_or("unknown pair");
    let stat = |value: &str, label: &str| {
        format!(
            "<div class=\"ministat\"><div class=\"mininum\">{value}</div>\
             <div class=\"minilab\">{label}</div></div>"
        )
    };
    let headline = format!(
        "<div class=\"herofoot\">{}{}{}{}{}</div>",
        stat(&pct(propamm.won, propamm.solved), "win rate (all solved orders)"),
        stat(&fmt_count(propamm.won), "wins"),
        stat(&fmt_usd(propamm.captured_flow_usd), "flow captured"),
        stat(&fmt_bps(propamm.median_headroom_bps), "median fee headroom bps"),
        stat(&fmt_bps(propamm.avg_headroom_bps()), "fee headroom bps (flow-weighted)"),
    );
    let body = format!(
        "<p class=\"note\">Mirrored pool: <strong>{}</strong>, priced fee-free. \
         <em>Fee headroom</em> is what the signed extension could have charged and still beaten the \
         public market — measured, not assumed. Quotes from this pool are not executable.</p>\
         {headline}{}",
        escape(pair),
        propamm_pair_table(&propamm.by_pair),
    );
    section("Mock PropAMM (exclusive route)", &body)
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
        assert!(html.contains("median fee headroom bps"));
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
}
