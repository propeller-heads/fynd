use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

mod golden;
mod recorder;

#[derive(Parser)]
#[command(name = "record-market", about = "Capture Tycho market state for integration testing")]
struct Cli {
    /// Tycho WebSocket URL
    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    /// Tycho API key
    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: String,

    /// Ethereum RPC URL for gas price capture.
    /// If provided, the gas price is recorded and used during replay.
    #[arg(long, env = "RPC_URL")]
    rpc_url: Option<String>,

    /// Duration to record stream updates (seconds)
    #[arg(long, default_value = "600")]
    duration_secs: u64,

    /// Output directory for fixtures
    #[arg(long, default_value = "fixtures/integration")]
    output_dir: PathBuf,

    /// Protocol systems to record (e.g. uniswap_v2, uniswap_v3).
    /// If omitted, all protocols discovered from Tycho are used.
    #[arg(long, value_delimiter = ',')]
    protocols: Option<Vec<String>>,

    /// Minimum TVL in native token (ETH) for component filtering.
    #[arg(long, default_value = "10.0")]
    min_tvl: f64,

    /// Minimum token quality score.
    #[arg(long, default_value = "100")]
    min_token_quality: i32,

    /// Only include tokens traded within this many days.
    /// Defaults to 3 (same as production).
    #[arg(long, default_value = "3")]
    traded_n_days_ago: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    tracing::info!("connecting to Tycho at {}", cli.tycho_url);

    let recording_opts = recorder::RecordingOptions {
        tycho_url: cli.tycho_url,
        tycho_api_key: cli.tycho_api_key,
        duration_secs: cli.duration_secs,
        protocols: cli.protocols,
        min_tvl: cli.min_tvl,
        min_token_quality: cli.min_token_quality,
        traded_n_days_ago: cli.traded_n_days_ago,
        rpc_url: cli.rpc_url,
    };

    // 1. Record Update messages from live Tycho stream
    let recording = recorder::record_market(&recording_opts).await?;

    tracing::info!(
        updates = recording.updates.len(),
        duration_s = recording.metadata.recording_duration_s,
        "market recording captured"
    );

    // 2. Write recording
    std::fs::create_dir_all(&cli.output_dir)?;
    let recording_path = cli
        .output_dir
        .join("market_recording.json.zst");
    fynd_core::recording::write_recording(&recording, &recording_path)?;
    tracing::info!(path = %recording_path.display(), "recording written");

    // 3. Generate golden outputs by replaying the recording
    let golden = golden::generate_golden_outputs(recording).await?;
    let golden_path = cli
        .output_dir
        .join("golden_outputs.json");
    let golden_json = serde_json::to_string_pretty(&golden)?;
    std::fs::write(&golden_path, golden_json)?;
    tracing::info!(
        scenarios = golden.scenarios.len(),
        path = %golden_path.display(),
        "golden outputs written"
    );

    Ok(())
}
