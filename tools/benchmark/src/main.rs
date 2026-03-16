mod benchmark;
mod compare;
mod config;
mod exporter;
mod requests;
mod runner;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "fynd-benchmark")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load-test a running Fynd solver (latency and throughput)
    Load(benchmark::Args),
    /// Compare output quality between two running Fynd solvers
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
