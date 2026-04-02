//! Leaderboard aggregation and rendering.

use std::path::Path;

use anyhow::Result;

/// Reads score JSON files from `scores_dir`, sorts by score descending, and prints a table.
///
/// # Errors
///
/// Returns an error if the directory cannot be read or any score file is malformed.
pub fn print_leaderboard(_scores_dir: &Path) -> Result<()> {
    todo!("read score files, sort by score descending, render table")
}
