use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

mod expected;
mod recorder;

#[derive(Parser)]
#[command(name = "record-market", about = "Capture Tycho market state for integration testing")]
struct Cli {
    /// Tycho WebSocket URL.
    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    /// Tycho API key.
    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: String,

    /// Chain RPC URL for gas price capture.
    #[arg(long, env = "RPC_URL")]
    rpc_url: Option<String>,

    /// Chain to record (e.g. "ethereum", "base", "unichain").
    #[arg(long, default_value = "ethereum")]
    chain: String,

    /// Duration to record stream updates (seconds).
    #[arg(long, default_value = "600")]
    duration_secs: u64,

    /// Output directory for fixtures.
    #[arg(long, default_value = "fynd-core/tests/fixtures")]
    output_dir: PathBuf,

    /// Protocol systems to record (comma-delimited).
    #[arg(long, value_delimiter = ',')]
    protocols: Option<Vec<String>>,

    /// Minimum TVL in ETH for component filtering.
    #[arg(long, default_value = "10.0")]
    min_tvl: f64,

    /// Minimum token quality score.
    #[arg(long, default_value = "100")]
    min_token_quality: i32,

    /// Only include tokens traded within this many days.
    #[arg(long, default_value = "3")]
    traded_n_days_ago: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let chain = fynd_core::types::parse_chain(&cli.chain)?;
    // parse_chain lowercases its input, so a successful parse means the
    // lowercased name is the canonical serde representation of the chain.
    let chain_name = cli.chain.to_ascii_lowercase();

    tracing::info!(chain = %chain_name, "connecting to Tycho at {}", cli.tycho_url);

    let recording_opts = recorder::RecordingOptions {
        tycho_url: cli.tycho_url,
        tycho_api_key: cli.tycho_api_key,
        duration_secs: cli.duration_secs,
        protocols: cli.protocols,
        min_tvl: cli.min_tvl,
        min_token_quality: cli.min_token_quality,
        traded_n_days_ago: cli.traded_n_days_ago,
        rpc_url: cli.rpc_url,
        chain,
        chain_name,
    };

    let recording = recorder::record_market(&recording_opts).await?;

    tracing::info!(
        updates = recording.updates.len(),
        duration_s = recording
            .metadata
            .recording_duration_secs,
        "market recording captured"
    );

    std::fs::create_dir_all(&cli.output_dir)?;
    let recording_path = cli
        .output_dir
        .join("market_recording.json.zst");
    fynd_test_fixtures::write_recording(&recording, &recording_path)?;
    tracing::info!(path = %recording_path.display(), "recording written");

    // Read back from disk so expected output generation uses the same
    // deserialized data that integration tests will see (VM states filtered
    // during serialization won't be present in the deserialized version).
    let recording = fynd_test_fixtures::read_recording(&recording_path)?;

    let pools_toml = include_str!("../../../fynd.toml");
    let pairs_path =
        PathBuf::from(format!("fynd-core/tests/fixtures/pairs/{}.json", recording.metadata.chain));
    let pairs_json = std::fs::read_to_string(&pairs_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read scenario pairs file {}: {e} — add a pairs file for this chain \
             and run from the repo root",
            pairs_path.display()
        )
    })?;
    let expected = expected::generate_expected_outputs(recording, pools_toml, &pairs_json).await?;
    let expected_path = cli
        .output_dir
        .join("expected_outputs.json");
    let json = serde_json::to_string_pretty(&expected)?;
    std::fs::write(&expected_path, json)?;
    tracing::info!(
        scenarios = expected.scenarios.len(),
        path = %expected_path.display(),
        "expected outputs written"
    );

    Ok(())
}
