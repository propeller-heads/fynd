//! Runs a submission or baseline against a market snapshot and writes scores.
// `load_snapshot` will be used once the snapshot API is available.
#![allow(dead_code)]

use std::path::Path;

use anyhow::Result;
use fynd_algo_sdk::{SharedDerivedDataRef, SharedMarketDataRef};

/// Loads a market snapshot from disk.
///
/// Returns the shared market data and derived data references needed to run a worker pool.
fn load_snapshot(_path: &Path) -> Result<(SharedMarketDataRef, SharedDerivedDataRef)> {
    todo!("waiting for fynd-core snapshot API")
}

/// Runs a participant submission against a snapshot and writes scores to `output`.
///
/// # Errors
///
/// Returns an error if the library cannot be loaded, the snapshot is malformed, or
/// writing the output file fails.
pub fn run_submission(_submission: &Path, _snapshot: &Path, _output: &Path) -> Result<()> {
    todo!("load submission via loader, build WorkerPool, feed orders, score, write output")
}

/// Runs the built-in baseline algorithm against a snapshot and writes scores to `output`.
///
/// # Errors
///
/// Returns an error if the snapshot is malformed or writing the output file fails.
pub fn run_baseline(_snapshot: &Path, _output: &Path) -> Result<()> {
    todo!("build WorkerPool with default algorithm, feed orders, score, write output")
}
