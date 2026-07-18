//! CLI entry point for fynd-benchmark.
//!
//! Dispatches to the `load` (latency/throughput) and `compare`
//! (output-quality diff) subcommands.

mod aggregator;
mod audit;
mod benchmark;
mod capacity;
mod capacity_report;
mod compare;
mod config;
mod exporter;
mod generate_requests;
mod pair_selector;
mod requests;
mod runner;
mod scale;

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
    /// Download the full 10k aggregator trade dataset from GitHub Releases
    DownloadTrades(DownloadTradesArgs),
    /// Benchmark throughput scaling across different worker counts
    Scale(scale::Args),
    /// Compare Fynd quote performance against external DEX aggregators
    Audit(audit::Args),
    /// Measure the highest sustainable request rate within the latency SLO
    Capacity(capacity::Args),
    /// Generate a synthetic per-chain request dataset for capacity testing
    GenerateRequests(generate_requests::Args),
}

/// Download the full 10k aggregator trade dataset for benchmarking.
#[derive(clap::Parser, Debug)]
pub struct DownloadTradesArgs {
    /// Output file path
    #[arg(short, long, default_value = "aggregator_trades_10k.json")]
    pub output: String,
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
        Command::DownloadTrades(args) => requests::download_trades(&args.output)
            .await
            .map_err(|e| anyhow::anyhow!("{e}")),
        Command::Scale(args) => scale::run(args).await,
        Command::Audit(args) => audit::run(args).await,
        Command::Capacity(args) => capacity::run(args).await,
        Command::GenerateRequests(args) => generate_requests::run(args).await,
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn load_defaults() {
        let cli = Cli::try_parse_from(["bin", "load"]).unwrap();
        let Command::Load(args) = cli.command else {
            panic!("expected Load");
        };
        assert_eq!(args.solver_url, "http://localhost:3000");
        assert_eq!(args.num_requests, 1);
        assert_eq!(args.parallelization_mode, "sequential");
        assert!(args.requests_file.is_none());
        assert!(args.output_file.is_none());
        assert!(!args.encoding);
        assert!(args.api_key.is_none());
        assert_eq!(args.quote_path, "/v1/quote");
        assert_eq!(args.health_path, "/v1/health");
    }

    #[test]
    fn load_all_args() {
        let cli = Cli::try_parse_from([
            "bin",
            "load",
            "--solver-url",
            "http://x:9000",
            "-n",
            "50",
            "-m",
            "fixed:4",
            "--requests-file",
            "f.json",
            "--output-file",
            "o.json",
            "--encoding",
        ])
        .unwrap();
        let Command::Load(args) = cli.command else {
            panic!("expected Load");
        };
        assert_eq!(args.solver_url, "http://x:9000");
        assert_eq!(args.num_requests, 50);
        assert_eq!(args.parallelization_mode, "fixed:4");
        assert_eq!(args.requests_file.as_deref(), Some("f.json"));
        assert_eq!(args.output_file.as_deref(), Some("o.json"));
        assert!(args.encoding);
    }

    #[test]
    fn compare_defaults() {
        let cli = Cli::try_parse_from(["bin", "compare"]).unwrap();
        let Command::Compare(args) = cli.command else {
            panic!("expected Compare");
        };
        assert_eq!(args.url_a, "http://localhost:3000");
        assert_eq!(args.url_b, "http://localhost:3001");
        assert_eq!(args.num_requests, 500);
        assert_eq!(args.timeout_ms, 15000);
        assert_eq!(args.seed, 42);
    }

    #[test]
    fn compare_all_args() {
        let cli = Cli::try_parse_from([
            "bin",
            "compare",
            "--url-a",
            "http://a",
            "--url-b",
            "http://b",
            "--label-a",
            "v1",
            "--label-b",
            "v2",
            "-n",
            "10",
            "--timeout-ms",
            "5000",
            "--seed",
            "99",
            "--output",
            "out.json",
        ])
        .unwrap();
        let Command::Compare(args) = cli.command else {
            panic!("expected Compare");
        };
        assert_eq!(args.url_a, "http://a");
        assert_eq!(args.url_b, "http://b");
        assert_eq!(args.label_a, "v1");
        assert_eq!(args.label_b, "v2");
        assert_eq!(args.num_requests, 10);
        assert_eq!(args.timeout_ms, 5000);
        assert_eq!(args.seed, 99);
        assert_eq!(args.output, "out.json");
    }

    #[test]
    fn capacity_defaults() {
        let cli = Cli::try_parse_from(["bin", "capacity"]).unwrap();
        let Command::Capacity(args) = cli.command else {
            panic!("expected Capacity");
        };
        assert_eq!(args.solver_url, "http://localhost:3000");
        assert_eq!(args.ladder, "5:5:200");
        assert_eq!(args.step_duration_secs, 60);
        assert_eq!(args.warmup_secs, 5);
        assert_eq!(args.baseline_requests, 50);
        assert!((args.slo_multiplier - 1.2).abs() < f64::EPSILON);
        assert!((args.max_http_error_rate - 0.001).abs() < f64::EPSILON);
        assert!((args.max_excess_unsolved_rate - 0.001).abs() < f64::EPSILON);
        assert_eq!(args.timeout_ms, 5000);
        assert!(!args.encoding);
        assert_eq!(args.seed, 42);
        assert!(args.api_key.is_none());
        assert_eq!(args.quote_path, "/v1/quote");
        assert_eq!(args.health_path, "/v1/health");
    }

    #[test]
    fn capacity_gateway_args() {
        let cli = Cli::try_parse_from([
            "bin",
            "capacity",
            "--solver-url",
            "https://fynd-api.propellerheads.xyz/v1/base",
            "--api-key",
            "secret-token",
            "--quote-path",
            "/quote",
            "--health-path",
            "/health",
        ])
        .unwrap();
        let Command::Capacity(args) = cli.command else {
            panic!("expected Capacity");
        };
        assert_eq!(args.solver_url, "https://fynd-api.propellerheads.xyz/v1/base");
        assert_eq!(args.api_key.as_deref(), Some("secret-token"));
        assert_eq!(args.quote_path, "/quote");
        assert_eq!(args.health_path, "/health");
    }

    #[test]
    fn capacity_max_excess_unsolved_rate_arg() {
        let cli =
            Cli::try_parse_from(["bin", "capacity", "--max-excess-unsolved-rate", "0.05"]).unwrap();
        let Command::Capacity(args) = cli.command else {
            panic!("expected Capacity");
        };
        assert!((args.max_excess_unsolved_rate - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn no_subcommand_errors() {
        assert!(Cli::try_parse_from(["bin"]).is_err());
    }

    #[test]
    fn unknown_subcommand_errors() {
        assert!(Cli::try_parse_from(["bin", "unknown"]).is_err());
    }
}
