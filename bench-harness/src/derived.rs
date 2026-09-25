//! Times the derived computations (spot prices, token prices, pool depths) on a recorded market.
//!
//! Reads and decodes the recording once, then replays it `--repeats` times, each time into a new
//! market and store, through `fynd_core::derived::bench::time_derived_computations`. More repeats
//! give a profiler more samples. Wrap it in samply with `./scripts/derived-bench.sh`.

use std::{path::PathBuf, time::Instant};

use clap::Parser;
use fynd_core::derived::bench::{time_derived_computations, DerivedBenchSettings};
use fynd_test_fixtures::read_recording;

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

    /// Emit fynd-core's logs. `RUST_LOG` picks the filter, e.g.
    /// `RUST_LOG=fynd_core::derived=debug`.
    #[arg(long)]
    logs: bool,
}

/// Parses the command line and runs the timing.
///
/// # Errors
///
/// When the recording cannot be read or decoded, or names a chain this build does not support.
pub async fn run() -> Result<(), String> {
    let args = Args::parse();
    crate::init_logging(args.logs);

    let start = Instant::now();
    let path = args.recording.display();
    let recording = read_recording(&args.recording)
        .map_err(|error| format!("cannot read the recording at {path}: {error:#}"))?;
    let chain = fynd_core::types::parse_chain(&recording.metadata.chain)
        .map_err(|error| format!("{path}: {error}"))?;
    let updates = recording
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
    };
    for run in 1..=args.repeats {
        println!("run {run}/{}", args.repeats);
        time_derived_computations(&settings, updates.clone()).await;
    }
    Ok(())
}
