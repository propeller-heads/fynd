//! Render an aggregated [`Report`] to a single self-contained HTML file.
//!
//! No external assets or network: the verdict pie is a CSS `conic-gradient`, styling is one inline
//! stylesheet, so the file opens offline. The panels mirror the Grafana dashboard's value views —
//! the headline Fynd savings (`hindsight_savings_usd`), the verdict split, coverage by notional,
//! per-solver/venue breakdowns, and the top-saving trades — and skip the block/latency health
//! panels the JSONL does not carry.

use std::fmt::Write as _;

use crate::report::aggregate::{
    Count, GroupStats, Report, Savings, Summary, TradeRow, VerdictStat,
};

/// Colours for each verdict, shared by the pie segments and their legend swatches.
fn verdict_color(verdict: &str) -> &'static str {
    match verdict {
        "win" => "#43a047",
        "loss" => "#e53935",
        "unsolvable" => "#fbc02d",
        "coverage_miss" => "#fb8c00",
        "sandwiched" => "#8e24aa",
        _ => "#9e9e9e",
    }
}

/// Render the whole report to an HTML document. `filter` names the active venue filter, if any, so
/// the report says which slice of trades it covers.
pub(crate) fn render(report: &Report, filter: Option<&str>) -> String {
    let mut html = String::from(HEAD);
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

/// The headline: Fynd's savings as the uplift on trades it wins (`hindsight_improvement_usd`), one
/// number, no charts. The loss drag and net are on the sub-line for context — kept off the headline
/// because a single bad-liquidity snapshot can dominate the signed net — and each loss is listed in
/// the biggest-losses table.
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
    format!(
        "<section class=\"hero\">\
           <div class=\"heroscope\">hindsight report {scope}</div>\
           <div class=\"herostats\">{}{}{}</div>\
           <div class=\"herosub\">Fynd savings is the uplift on trades Fynd wins \
             (hindsight_improvement_usd). Across {} wins · {} given up on {} losses · \
             net {} · {} scored · {} comparisons / {} blocks</div>\
         </section>",
        big(&fmt_usd(savings.won_usd), "Fynd savings (wins uplift)", "pos big"),
        big(&pct(savings.wins, savings.scored), "win rate", "pos"),
        big(&fmt_bps_signed(savings.median_win_bps), "median savings bps (wins)", "pos"),
        savings.wins,
        fmt_usd(savings.lost_usd),
        savings.losses,
        fmt_usd(savings.net_usd),
        savings.scored,
        summary.total,
        summary.distinct_blocks,
    )
}

/// The verdict split as two `conic-gradient` pies — one weighted by trade count, one by settled USD
/// volume — each with a legend, mirroring the dashboard's outcome-by-count and outcome-by-volume
/// panels.
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
        "<div class=\"pies\">\
           <div class=\"piecol\"><h3>by trade count</h3>{}</div>\
           <div class=\"piecol\"><h3>by volume (settled USD)</h3>{}</div>\
         </div>",
        pie(&by_count, |v| format!("{v:.0}")),
        pie(&by_volume, fmt_usd),
    );
    section("Verdicts (top-of-block)", &body)
}

/// A `conic-gradient` pie with a legend, from `(verdict, value)` slices. `fmt_val` formats each
/// legend value (a count or a USD amount). Colours come from [`verdict_color`].
fn pie(entries: &[(&str, f64)], fmt_val: impl Fn(f64) -> String) -> String {
    let total: f64 = entries.iter().map(|(_, v)| v).sum();
    if total <= 0.0 {
        return "<p>no data</p>".to_string();
    }
    let mut segments = String::new();
    let mut legend = String::new();
    let mut acc = 0.0;
    for &(label, value) in entries {
        let color = verdict_color(label);
        let start = acc;
        acc += value / total * 100.0;
        if !segments.is_empty() {
            segments.push(',');
        }
        let _ = write!(segments, "{color} {start:.3}% {acc:.3}%");
        let _ = write!(
            legend,
            "<li><span class=\"swatch\" style=\"background:{color}\"></span>\
             <span class=\"legname\">{}</span>\
             <span class=\"legval\">{} ({:.1}%)</span></li>",
            escape(label),
            fmt_val(value),
            value / total * 100.0,
        );
    }
    format!(
        "<div class=\"pie-wrap\">\
           <div class=\"pie\" style=\"background:conic-gradient({segments})\"></div>\
           <ul class=\"legend\">{legend}</ul>\
         </div>",
    )
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
.heronum { font-size: 3.5rem; font-weight: 700; line-height: 1.05; letter-spacing: -0.02em; }
.heronum.big { font-size: 4.25rem; }
.heronum.pos { color: #66bb6a; }
.heronum.neg { color: #ef5350; }
.herolab { color: #9a8bbf; font-size: .8rem; text-transform: uppercase; letter-spacing: .04em; margin-top: .3rem; }
.herosub { color: #b9aecf; font-size: .9rem; margin-top: 1.25rem; }
.pies { display: flex; flex-wrap: wrap; gap: 2.5rem; }
.piecol { flex: 1 1 0; min-width: 20rem; }
.pie-wrap { display: flex; flex-wrap: wrap; align-items: center; gap: 1.5rem; }
.pie { width: 180px; height: 180px; border-radius: 50%; flex: 0 0 auto;
  box-shadow: inset 0 0 0 1px #362b4a; }
.legend { list-style: none; margin: 0; padding: 0; min-width: 14rem; }
.legend li { display: flex; align-items: center; gap: .6rem; padding: .25rem 0; }
.swatch { width: .85rem; height: .85rem; border-radius: 2px; flex: 0 0 auto; }
.legname { flex: 1; text-transform: capitalize; }
.legval { color: #b9aecf; font-variant-numeric: tabular-nums; }
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
        // Verdicts render as two pies — by trade count and by volume — not bars, and not a
        // separate coverage table.
        assert!(html.contains("conic-gradient"));
        assert!(html.contains("by trade count") && html.contains("by volume"));
        assert!(!html.contains(">Coverage<"));
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
}
