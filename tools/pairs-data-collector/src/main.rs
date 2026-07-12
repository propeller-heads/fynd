use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pairs_data_collector::{
    config::CollectorConfig,
    runtime::{run_collect, CollectArgs},
    storage::compact_wal,
    validate::validate_wal,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "pairs-data-collector")]
#[command(about = "Collect block-level executable Fynd curves")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and validate a TOML configuration without reading secrets.
    CheckConfig {
        /// Configuration to validate.
        #[arg(long)]
        config: PathBuf,
    },
    /// Run the live Ethereum collector.
    Collect {
        /// Validated TOML configuration.
        #[arg(long)]
        config: PathBuf,
        /// Root directory for WAL and Parquet output.
        #[arg(long)]
        output_dir: PathBuf,
        /// Stop after this many live heads, useful for capacity probes.
        #[arg(long)]
        max_heads: Option<u64>,
    },
    /// Validate a durable JSONL WAL.
    Validate {
        /// WAL file to validate.
        #[arg(long)]
        wal: PathBuf,
    },
    /// Validate and compact a WAL into Parquet datasets.
    Compact {
        /// WAL file to compact.
        #[arg(long)]
        wal: PathBuf,
        /// Root output directory.
        #[arg(long)]
        output_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    match Cli::parse().command {
        Command::CheckConfig { config } => {
            let parsed = CollectorConfig::load(&config)?;
            info!(
                pairs = parsed.pairs.len(),
                expected_rows_per_block = parsed.expected_rows_per_block(),
                "collector configuration is valid"
            );
            Ok(())
        }
        Command::Collect { config, output_dir, max_heads } => {
            run_collect(CollectArgs { config_path: config, output_dir, max_heads }).await
        }
        Command::Validate { wal } => {
            let report = validate_wal(&wal)?;
            info!(?report, "WAL is structurally valid");
            Ok(())
        }
        Command::Compact { wal, output_dir } => {
            validate_wal(&wal)?;
            let report = compact_wal(&wal, &output_dir)?;
            info!(?report, "WAL compacted successfully");
            Ok(())
        }
    }
}
