//! Competition harness CLI for Fynd algorithm submissions.
//!
//! Subcommands:
//! - `run`         — load a submission `.so`, feed it the round snapshot, and emit scores
//! - `baseline`    — run the built-in reference algorithm and record its scores
//! - `leaderboard` — aggregate score files and render a sorted table

mod leaderboard;
mod loader;
mod runner;
mod scorer;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// Fynd competition harness.
#[derive(Parser)]
#[command(name = "fynd-competition", about = "Fynd algorithm competition harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a submission shared library and score it against the round snapshot.
    Run {
        /// Path to the participant's compiled `.so` / `.dylib`.
        #[arg(long)]
        submission: PathBuf,
        /// Path to the market snapshot file.
        #[arg(long)]
        snapshot: PathBuf,
        /// Path to write the score JSON output.
        #[arg(long)]
        output: PathBuf,
    },
    /// Run the built-in baseline algorithm and record its scores.
    Baseline {
        /// Path to the market snapshot file.
        #[arg(long)]
        snapshot: PathBuf,
        /// Path to write the baseline score JSON output.
        #[arg(long)]
        output: PathBuf,
    },
    /// Read score files and render the leaderboard table.
    Leaderboard {
        /// Directory containing score JSON files.
        #[arg(long)]
        scores_dir: PathBuf,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Run { submission, snapshot, output } => {
            runner::run_submission(&submission, &snapshot, &output)
        }
        Command::Baseline { snapshot, output } => runner::run_baseline(&snapshot, &output),
        Command::Leaderboard { scores_dir } => leaderboard::print_leaderboard(&scores_dir),
    }
}
