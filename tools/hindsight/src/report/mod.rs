//! Offline HTML report from a monitor run's comparison JSONL.
//!
//! Reads the `comparisons-YYYY-MM-DD.jsonl` files a `monitor --comparisons-dir` run wrote (all of
//! them, so a rotated multi-day run aggregates into one report), computes the same analytical views
//! as the Grafana dashboard, and writes a single self-contained HTML file. It touches no chain, no
//! Tycho, and no network — jsonl in, html out.

mod aggregate;
mod html;
mod record;

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use tracing::{info, warn};

use crate::{decoder::DecodeTier, report::record::Comparison};

/// Inputs for the `report` subcommand.
#[derive(clap::Args)]
pub(crate) struct ReportArgs {
    /// Directory of `comparisons-YYYY-MM-DD.jsonl` files written by `monitor --comparisons-dir`
    #[arg(long)]
    pub comparisons_dir: PathBuf,

    /// Output HTML path (defaults to `<comparisons-dir>/report.html`)
    #[arg(long, short)]
    pub output: Option<PathBuf>,

    /// Only report trades from these venues (repeatable, case-insensitive). Omit for all venues.
    #[arg(long)]
    pub venue: Vec<String>,

    /// Include netted records — trades whose amounts came from balance netting rather than the
    /// solver's own calldata or logs, so an unaccounted fee can sit inside them. Excluded by
    /// default; datasets recorded before the marker existed are always included.
    #[arg(long)]
    pub include_netted: bool,
}

/// Read the comparisons, aggregate them, and write the HTML report.
pub(crate) fn run(args: ReportArgs) -> anyhow::Result<()> {
    let all = read_comparisons(&args.comparisons_dir)?;
    if all.is_empty() {
        bail!("no comparison records found in {}", args.comparisons_dir.display());
    }
    let records = filter_netted(all, args.include_netted);
    let records = filter_by_venue(records, &args.venue)?;
    let report = aggregate::build(&records);
    let filter = (!args.venue.is_empty()).then(|| args.venue.join(", "));
    let html = html::render(&report, filter.as_deref());
    let output = args
        .output
        .unwrap_or_else(|| args.comparisons_dir.join("report.html"));
    fs::write(&output, html)
        .with_context(|| format!("failed to write report to {}", output.display()))?;
    info!(records = records.len(), venue = ?args.venue, path = %output.display(), "wrote report");
    Ok(())
}

/// Drop the marked netted records unless `include_netted` asks for them. A record with no
/// `decode` column predates the marker and is kept either way.
fn filter_netted(records: Vec<Comparison>, include_netted: bool) -> Vec<Comparison> {
    if include_netted {
        return records;
    }
    records
        .into_iter()
        .filter(|record| record.decode.as_deref() != Some(DecodeTier::Netted.wire()))
        .collect()
}

/// Keep only the records whose venue is in `venues` (case-insensitive); all records when `venues`
/// is empty. Errors with the available venue list when the filter matches nothing, so a typo is
/// obvious rather than yielding a blank report.
fn filter_by_venue(records: Vec<Comparison>, venues: &[String]) -> anyhow::Result<Vec<Comparison>> {
    if venues.is_empty() {
        return Ok(records);
    }
    let wanted: Vec<String> = venues
        .iter()
        .map(|v| v.to_lowercase())
        .collect();
    let filtered: Vec<Comparison> = records
        .iter()
        .filter(|r| wanted.contains(&r.venue.to_lowercase()))
        .cloned()
        .collect();
    if filtered.is_empty() {
        // List the named venues, the sensible filter targets; unknown venues appear only as raw
        // addresses and would just be noise.
        let mut named: Vec<&str> = records
            .iter()
            .map(|r| r.venue.as_str())
            .filter(|venue| !venue.starts_with("0x"))
            .collect();
        named.sort_unstable();
        named.dedup();
        bail!("no trades match venue {venues:?}; available venues: {}", named.join(", "));
    }
    Ok(filtered)
}

/// Read and parse every `.jsonl` file in `dir` into comparison records. Malformed lines are
/// counted and skipped rather than failing the whole report — a truncated final line from an
/// interrupted run should not lose the rest of the data.
fn read_comparisons(dir: &Path) -> anyhow::Result<Vec<Comparison>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("failed to read comparisons directory {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
        })
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no .jsonl files in {}", dir.display());
    }

    let mut records = Vec::new();
    let mut skipped = 0usize;
    for file in &files {
        let content = fs::read_to_string(file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Comparison>(line) {
                Ok(record) => records.push(record),
                Err(_) => skipped += 1,
            }
        }
    }
    if skipped > 0 {
        warn!(skipped, files = files.len(), "skipped malformed comparison lines");
    }
    info!(files = files.len(), records = records.len(), "read comparisons");
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hindsight-report-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (file, content) in files {
            fs::write(dir.join(file), content).unwrap();
        }
        dir
    }

    fn line(block: u64, verdict: &str) -> String {
        serde_json::json!({
            "block": block, "settled_tx": format!("0x{block:064x}"),
            "venue": "relay", "solver": "1inch", "token_in": "0xaaa", "token_out": "0xbbb",
            "top": {"verdict": verdict, "raw_bps": 1.0, "improvement_usd": 1.0, "settled_value_usd": 1.0},
        })
        .to_string()
    }

    #[test]
    fn test_reads_all_jsonl_files_and_skips_malformed_lines() {
        let dir = write_dir(
            "multi",
            &[
                (
                    "comparisons-2026-07-20.jsonl",
                    &format!("{}\n{}\n", line(1, "win"), line(2, "loss")),
                ),
                // A blank line, a good line, and a truncated one.
                (
                    "comparisons-2026-07-21.jsonl",
                    &format!("\n{}\n{{\"block\":3", line(3, "unsolvable")),
                ),
                ("report.html", "<html></html>"), // ignored: not .jsonl
            ],
        );
        let records = read_comparisons(&dir).unwrap();
        assert_eq!(records.len(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_empty_dir_is_an_error() {
        let dir = write_dir("empty", &[]);
        assert!(read_comparisons(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn venue_record(venue: &str) -> Comparison {
        serde_json::from_value(serde_json::json!({
            "block": 1, "settled_tx": "0x1", "venue": venue, "solver": "1inch",
            "token_in": "0xaaa", "token_out": "0xbbb",
            "top": {"verdict": "win", "raw_bps": 1.0, "improvement_usd": 1.0, "settled_value_usd": 1.0},
        }))
        .unwrap()
    }

    fn decode_record(decode: Option<&str>) -> Comparison {
        let mut record = serde_json::json!({
            "block": 1, "settled_tx": "0x1", "venue": "relay", "solver": "1inch",
            "token_in": "0xaaa", "token_out": "0xbbb",
            "top": {"verdict": "win", "raw_bps": 1.0, "improvement_usd": 1.0, "settled_value_usd": 1.0},
        });
        if let Some(decode) = decode {
            record["decode"] = decode.into();
        }
        serde_json::from_value(record).unwrap()
    }

    #[test]
    fn test_filter_netted() {
        let records = || {
            vec![
                decode_record(Some("declared")),
                decode_record(Some("netted")),
                // Recorded before the marker existed: no column, kept either way.
                decode_record(None),
            ]
        };
        let kept = filter_netted(records(), false);
        assert_eq!(kept.len(), 2);
        assert!(kept
            .iter()
            .all(|record| record.decode.as_deref() != Some("netted")));
        assert_eq!(filter_netted(records(), true).len(), 3);
    }

    #[test]
    fn test_filter_by_venue() {
        let records = vec![venue_record("relay"), venue_record("metamask"), venue_record("relay")];
        // No filter keeps everything.
        assert_eq!(
            filter_by_venue(records.clone(), &[])
                .unwrap()
                .len(),
            3
        );
        // Case-insensitive match on the named venue(s).
        assert_eq!(
            filter_by_venue(records.clone(), &["RELAY".into()])
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            filter_by_venue(records.clone(), &["relay".into(), "metamask".into()])
                .unwrap()
                .len(),
            3
        );
        // An unmatched venue errors with the available list, not a blank report.
        let err = filter_by_venue(records, &["cow".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("metamask") && err.contains("relay"), "{err}");
    }
}
