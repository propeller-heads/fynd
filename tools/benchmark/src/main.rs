//! CLI entry point for fynd-benchmark.
//!
//! Dispatches to the `load` (latency/throughput) and `compare`
//! (output-quality diff) subcommands.

mod benchmark;
mod compare;
mod config;
mod exporter;
mod requests;
mod runner;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "fynd-benchmark",
    about = "Benchmark and compare Fynd solver instances",
    long_about = "Benchmark and compare Fynd solver instances.\n\n\
        Requires at least one running Fynd solver. See the quickstart guide for setup."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure solver latency and throughput under load
    Load(benchmark::Args),
    /// Diff output quality (amount out, gas, routes) between two solvers
    Compare(compare::Args),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("RUST_LOG"))
        .with_target(true)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Load(args) => benchmark::run(args).await,
        Command::Compare(args) => compare::run(args).await,
    }
}
