use std::time::{Duration, Instant};

use fynd_rpc::protocols::fetch_protocol_systems;
use fynd_test_fixtures::{MarketRecording, RecordingMetadata};
use tokio_stream::StreamExt;
use tycho_simulation::{
    evm::stream::ProtocolStreamBuilder, protocol::models::Update,
    tycho_client::feed::component_tracker::ComponentFilter, tycho_common::models::Chain,
    tycho_core::traits::FeePriceGetter, tycho_ethereum::rpc::EthereumRpcClient,
    utils::load_all_tokens,
};

pub struct RecordingOptions {
    pub tycho_url: String,
    pub tycho_api_key: String,
    pub duration_secs: u64,
    pub protocols: Option<Vec<String>>,
    pub min_tvl: f64,
    pub min_token_quality: i32,
    pub traded_n_days_ago: u64,
    pub rpc_url: Option<String>,
    pub chain: Chain,
    /// Canonical (lowercase serde) chain name, stored in the recording metadata.
    pub chain_name: String,
}

/// Connect to Tycho, capture raw Update messages for the configured
/// duration, and return a MarketRecording.
pub async fn record_market(opts: &RecordingOptions) -> anyhow::Result<MarketRecording> {
    let chain = opts.chain;

    let protocols = match &opts.protocols {
        Some(p) if !p.is_empty() => {
            tracing::info!(protocols = ?p, "using explicit protocol list");
            p.clone()
        }
        _ => {
            let discovered =
                fetch_protocol_systems(&opts.tycho_url, Some(&opts.tycho_api_key), true, chain)
                    .await?;
            tracing::info!(count = discovered.len(), ?discovered, "discovered protocols");
            discovered
        }
    };

    let all_tokens = load_all_tokens(
        &opts.tycho_url,
        false,
        Some(&opts.tycho_api_key),
        true,
        chain,
        Some(opts.min_token_quality),
        Some(opts.traded_n_days_ago),
    )
    .await?;
    tracing::info!(count = all_tokens.len(), "loaded tokens");

    let gas_price_wei = match &opts.rpc_url {
        Some(url) => match fetch_gas_price_wei(url).await {
            Ok(wei) => {
                tracing::info!(gas_price_wei = %wei, "captured gas price from RPC");
                Some(wei)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch gas price");
                None
            }
        },
        None => None,
    };

    let tvl_filter = ComponentFilter::with_tvl_range(opts.min_tvl, opts.min_tvl);
    let builder = ProtocolStreamBuilder::new(&opts.tycho_url, chain);

    let builder = fynd_core::feed::protocol_registry::register_exchanges_for_recording(
        builder, tvl_filter, &protocols,
    )
    .map_err(|e| anyhow::anyhow!("failed to register exchanges: {e}"))?;

    let mut stream = Box::pin(
        builder
            .auth_key(Some(opts.tycho_api_key.clone()))
            .skip_state_decode_failures(true)
            .set_tokens(all_tokens)
            .await
            .build()
            .await?,
    );

    let mut updates: Vec<Update> = Vec::new();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(opts.duration_secs);

    tracing::info!(duration_secs = opts.duration_secs, "recording...");

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(update))) => {
                tracing::debug!(
                    block = update.block_number_or_timestamp,
                    new_pairs = update.new_pairs.len(),
                    states = update.states.len(),
                    "captured update"
                );
                updates.push(update);
            }
            Ok(Some(Err(e))) => tracing::warn!("stream error (continuing): {e}"),
            Ok(None) => {
                tracing::info!("stream ended");
                break;
            }
            Err(_) => {
                tracing::info!("recording duration reached");
                break;
            }
        }
    }

    let actual_duration = start.elapsed().as_secs();
    tracing::info!(updates = updates.len(), actual_duration, "recording complete");

    let pools_toml = include_str!("../../../fynd.toml");
    let worker_pools_hash = fynd_test_fixtures::recording::sha256_hex(pools_toml.as_bytes());

    Ok(MarketRecording {
        metadata: RecordingMetadata {
            chain: opts.chain_name.clone(),
            recorded_at_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time went backwards")
                .as_secs(),
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
            recording_duration_secs: actual_duration,
            protocols,
            min_tvl: opts.min_tvl,
            min_token_quality: opts.min_token_quality,
            traded_n_days_ago: Some(opts.traded_n_days_ago),
            gas_price_wei,
            worker_pools_hash: Some(worker_pools_hash),
            schema_version: 1,
        },
        updates,
    })
}

async fn fetch_gas_price_wei(rpc_url: &str) -> anyhow::Result<String> {
    let client = EthereumRpcClient::new(rpc_url)
        .map_err(|e| anyhow::anyhow!("failed to create RPC client: {e}"))?;
    let block_gas_price = client
        .get_latest_fee_price()
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch gas price: {e}"))?;
    tracing::info!(
        ?block_gas_price.pricing,
        "fetched gas price from RPC"
    );
    Ok(block_gas_price
        .effective_gas_price()
        .to_string())
}
