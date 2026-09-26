//! Times the derived computations (spot prices, token prices, pool depths) on a recorded market.
//!
//! Reads and decodes the recording once, then replays it `--repeats` times, each time into a new
//! market and store, through `fynd_core::derived::bench::time_derived_computations`. More repeats
//! give a profiler more samples. Wrap it in samply with `./scripts/derived-bench.sh`.
//!
//! `--write-snapshot` saves the token prices and pool depths after the first and the last block to
//! a snapshot file. `--check-snapshot` compares a run with a saved snapshot file and fails when a
//! value differs by more than its tolerance. A faster version of a computation uses it to show that
//! it computes the same values.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clap::Parser;
use fynd_core::derived::bench::{
    megabytes, time_derived_computations, ComputationRun, DerivedBenchRuns, DerivedBenchSettings,
    ReplayValues,
};
use fynd_test_fixtures::read_recording;

/// The largest relative difference a token price may have from the snapshot.
const PRICE_TOLERANCE: f64 = 0.001;
/// The largest relative difference a pool depth may have from the snapshot.
const DEPTH_TOLERANCE: f64 = 0.01;
/// The largest share of the snapshot file's keys that may be in only one of the file and this run.
const MISSING_KEY_TOLERANCE: f64 = 0.005;

#[derive(Parser, Debug)]
#[command(about = "Time every derived computation on a recorded market, block by block")]
struct Args {
    /// Market recording to replay, as `tools/record-market` writes it.
    #[arg(long)]
    recording: PathBuf,

    /// How many times to replay the whole recording, each time into a new market and store.
    #[arg(long, default_value_t = 1)]
    repeats: usize,

    /// The hop limit for `token_prices`, as `fynd serve --pricing-max-hops` sets it. The default
    /// is production's.
    #[arg(long, default_value_t = fynd_core::solver::defaults::PRICING_MAX_HOPS)]
    pricing_max_hops: usize,

    /// Save the token prices and pool depths after the first and the last block of the last replay
    /// to this JSON file.
    #[arg(long)]
    write_snapshot: Option<PathBuf>,

    /// Compare the token prices and pool depths of the last replay with this JSON file, and fail
    /// when they differ by more than the tolerances.
    #[arg(long)]
    check_snapshot: Option<PathBuf>,

    /// Emit fynd-core's logs. `RUST_LOG` picks the filter, e.g.
    /// `RUST_LOG=fynd_core::derived=debug`.
    #[arg(long)]
    logs: bool,
}

/// Parses the command line and runs the timing.
///
/// # Errors
///
/// When the recording cannot be read or decoded, the recording names a chain this build does not
/// support, a snapshot file cannot be read or written, or the snapshot check fails.
pub async fn run() -> Result<(), String> {
    let args = Args::parse();
    crate::init_logging(args.logs);

    let start = Instant::now();
    let path = args.recording.display();
    let recording = read_recording(&args.recording)
        .map_err(|error| format!("cannot read the recording at {path}: {error:#}"))?;
    let chain = fynd_core::types::parse_chain(&recording.metadata.chain)
        .map_err(|error| format!("{path}: {error}"))?;
    let mut updates = recording
        .decode_updates()
        .await
        .map_err(|error| format!("cannot decode the recording at {path}: {error:#}"))?;
    println!(
        "read and decoded {} messages from {path} in {:.1} s",
        updates.len(),
        start.elapsed().as_secs_f64()
    );

    let settings = DerivedBenchSettings {
        chain,
        pricing_max_hops: args.pricing_max_hops,
        gas_price_wei: recording
            .metadata
            .gas_price_as_biguint(),
        heap_probe: Some(crate::heap::sample),
    };
    // The decoded updates hold everything the replay needs; the raw recording would only add to
    // the heap the bench measures.
    drop(recording);
    let mut last_report = None;
    for run in 1..=args.repeats {
        println!("run {run}/{}", args.repeats);
        // The last replay takes the updates instead of a copy, so no second copy of the market
        // sits in the heap it measures.
        let replayed =
            if run == args.repeats { std::mem::take(&mut updates) } else { updates.clone() };
        let report = time_derived_computations(&settings, replayed).await;
        print_summary(&report.runs);
        last_report = Some(report);
    }
    let Some(report) = last_report else {
        return Ok(());
    };
    if let Some(path) = &args.write_snapshot {
        write_snapshot(path, &report.values)?;
        println!("wrote the snapshot to {}", path.display());
    }
    if let Some(path) = &args.check_snapshot {
        check_snapshot(path, &report.values)?;
    }
    Ok(())
}

fn print_summary(runs: &DerivedBenchRuns) {
    if let Some(heap) = runs.market_heap {
        println!("heap: market after the snapshot replay {:.1} MB", megabytes(heap.live_bytes));
    }
    println!("summary: first block, then later blocks (count, mean, total)");
    for (id, first) in &runs.first_block {
        let later = runs
            .later_blocks
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let total: Duration = later
            .iter()
            .map(|run| run.elapsed)
            .sum();
        let mean_ms =
            if later.is_empty() { 0.0 } else { total.as_secs_f64() * 1000.0 / later.len() as f64 };
        println!(
            "  {id:<14} first {:>10.1} ms | later {:>4} blocks, mean {:>9.1} ms, total {:>10.1} ms",
            first.elapsed.as_secs_f64() * 1000.0,
            later.len(),
            mean_ms,
            total.as_secs_f64() * 1000.0,
        );
    }
    let Some(market) = runs.market_heap else {
        return;
    };
    println!("heap above the market: live after the computation, peak while it ran (MB)");
    for (id, first) in &runs.first_block {
        let later = runs
            .later_blocks
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let above_market = |bytes: usize| megabytes(bytes) - megabytes(market.live_bytes);
        let first_heap = first.heap.unwrap_or_default();
        let later_peak = later
            .iter()
            .filter_map(|run: &ComputationRun| run.heap)
            .map(|heap| heap.peak_bytes)
            .max()
            .unwrap_or_default();
        let last_live = later
            .last()
            .and_then(|run| run.heap)
            .map_or(first_heap.live_bytes, |heap| heap.live_bytes);
        println!(
            "  {id:<14} first: live {:>8.1} peak {:>8.1} | later: max peak {:>8.1}, last live {:>8.1}",
            above_market(first_heap.live_bytes),
            above_market(first_heap.peak_bytes),
            above_market(later_peak),
            above_market(last_live),
        );
    }
}

fn write_snapshot(path: &Path, values: &ReplayValues) -> Result<(), String> {
    let json = serde_json::to_string(values)
        .map_err(|error| format!("cannot serialise the snapshot: {error}"))?;
    std::fs::write(path, json)
        .map_err(|error| format!("cannot write the snapshot to {}: {error}", path.display()))
}

fn check_snapshot(path: &Path, actual: &ReplayValues) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read the snapshot at {}: {error}", path.display()))?;
    let expected: ReplayValues = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse the snapshot at {}: {error}", path.display()))?;
    let blocks = [
        ("first block", &expected.after_first_block, &actual.after_first_block),
        ("last block", &expected.after_last_block, &actual.after_last_block),
    ];
    let mut failures = Vec::new();
    for (block, expected, actual) in blocks {
        let checks = [
            ("token prices", &expected.token_prices, &actual.token_prices, PRICE_TOLERANCE),
            ("pool depths", &expected.pool_depths, &actual.pool_depths, DEPTH_TOLERANCE),
        ];
        for (values, expected, actual, tolerance) in checks {
            if let Err(failure) = compare(&format!("{block} {values}"), expected, actual, tolerance)
            {
                failures.push(failure);
            }
        }
    }
    if failures.is_empty() {
        println!("snapshot check passed against {}", path.display());
        return Ok(());
    }
    Err(format!("snapshot check failed:\n{}", failures.join("\n")))
}

/// Compares one map of this run, token prices or pool depths, with the same map in the snapshot
/// file. Every key in both maps must differ by at most `tolerance`, and at most
/// `MISSING_KEY_TOLERANCE` of the file's keys may be in only one map. A difference that is not a
/// number counts as outside the tolerance.
fn compare(
    name: &str,
    expected: &BTreeMap<String, f64>,
    actual: &BTreeMap<String, f64>,
    tolerance: f64,
) -> Result<(), String> {
    let mut outside = Vec::new();
    let mut only_expected = 0usize;
    for (key, expected_value) in expected {
        let Some(actual_value) = actual.get(key) else {
            only_expected += 1;
            continue;
        };
        let difference = relative_difference(*expected_value, *actual_value);
        if difference.is_nan() || difference > tolerance {
            outside.push((difference, key.as_str(), *expected_value, *actual_value));
        }
    }
    let only_actual = actual
        .keys()
        .filter(|key| !expected.contains_key(*key))
        .count();
    let keys = expected.len().max(1) as f64;
    let missing_share = (only_expected + only_actual) as f64 / keys;
    outside.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!(
        "  {name}: {} keys, {} outside {:.2}%, {only_expected} only in the snapshot, {only_actual} \
         only in this run",
        expected.len(),
        outside.len(),
        tolerance * 100.0
    );
    for (difference, key, expected_value, actual_value) in outside.iter().take(5) {
        println!(
            "    {key}: expected {expected_value:e}, got {actual_value:e} ({:.3}%)",
            difference * 100.0
        );
    }
    if outside.is_empty() && missing_share <= MISSING_KEY_TOLERANCE {
        return Ok(());
    }
    Err(format!(
        "{name}: {} values outside {:.2}%, {:.2}% of keys in only one run",
        outside.len(),
        tolerance * 100.0,
        missing_share * 100.0
    ))
}

fn relative_difference(expected: f64, actual: f64) -> f64 {
    if expected == actual {
        return 0.0;
    }
    (expected - actual).abs() / expected.abs().max(actual.abs())
}
