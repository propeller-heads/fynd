use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

mod golden;
mod recorder;

#[derive(Parser)]
#[command(name = "record-market", about = "Capture Tycho market state for integration testing")]
struct Cli {
    /// Tycho WebSocket URL
    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    /// Ethereum RPC URL
    #[arg(long, env = "RPC_URL")]
    rpc_url: String,

    /// Tycho API key
    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: String,

    /// Duration to record stream updates (seconds)
    #[arg(long, default_value = "600")]
    duration_secs: u64,

    /// Output directory for fixtures
    #[arg(long, default_value = "fixtures/integration")]
    output_dir: PathBuf,

    /// Chain (ethereum, base, etc.)
    #[arg(long, default_value = "ethereum")]
    chain: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    tracing::info!("connecting to Tycho at {}", cli.tycho_url);

    // 1. Record Update messages from live Tycho stream
    let recording = recorder::record_market(
        &cli.tycho_url,
        &cli.rpc_url,
        &cli.tycho_api_key,
        &cli.chain,
        cli.duration_secs,
    )
    .await?;

    tracing::info!(
        updates = recording.updates.len(),
        duration_s = recording.metadata.recording_duration_s,
        "market recording captured"
    );

    // 2. Write recording
    std::fs::create_dir_all(&cli.output_dir)?;
    let recording_path = cli.output_dir.join("market_recording.json.zst");
    fynd_core::recording::write_recording(&recording, &recording_path)?;
    tracing::info!(path = %recording_path.display(), "recording written");

    // 3. Generate golden outputs by replaying the recording
    let golden = golden::generate_golden_outputs(recording).await?;
    let golden_path = cli.output_dir.join("golden_outputs.json");
    let golden_json = serde_json::to_string_pretty(&golden)?;
    std::fs::write(&golden_path, golden_json)?;
    tracing::info!(
        scenarios = golden.scenarios.len(),
        path = %golden_path.display(),
        "golden outputs written"
    );

    Ok(())
}
