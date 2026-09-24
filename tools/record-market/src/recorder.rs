use std::time::{Duration, Instant};

use fynd_core::feed::protocol_registry::open_recording_stream;
use fynd_rpc::protocols::resolve_protocols;
use fynd_test_fixtures::{MarketRecording, RecordingMetadata, SCHEMA_VERSION};
use tycho_simulation::{
    tycho_client::feed::{component_tracker::ComponentFilter, dto},
    tycho_common::models::Chain,
    tycho_core::traits::FeePriceGetter,
    tycho_ethereum::rpc::EthereumRpcClient,
    utils::load_all_tokens,
};

pub struct RecordingOptions {
    pub tycho_url: String,
    pub tycho_api_key: String,
    pub duration_secs: u64,
    pub protocols: Vec<String>,
    pub min_tvl: f64,
    pub min_token_quality: i32,
    pub traded_n_days_ago: u64,
    pub rpc_url: Option<String>,
    pub chain: Chain,
    /// Canonical (lowercase serde) chain name, stored in the recording metadata.
    pub chain_name: String,
}

/// Connects to Tycho, records the raw feed messages for the configured duration, and returns a
/// [`MarketRecording`].
pub async fn record_market(opts: &RecordingOptions) -> anyhow::Result<MarketRecording> {
    let chain = opts.chain;

    // `fynd serve --protocols` also uses `resolve_protocols`, so `native_onchain`, `all_onchain`
    // and `exclude:` entries record the market a solver streams.
    let protocols =
        resolve_protocols(&opts.tycho_url, Some(&opts.tycho_api_key), true, chain, &opts.protocols)
            .await?;
    tracing::info!(count = protocols.len(), ?protocols, "resolved protocols");

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
    let (stream_handle, mut stream) = open_recording_stream(
        &opts.tycho_url,
        chain,
        opts.tycho_api_key.clone(),
        tvl_filter,
        &protocols,
    )
    .await
    .map_err(|e| anyhow::anyhow!("cannot open the Tycho stream: {e}"))?;

    let mut messages: Vec<dto::FeedMessage> = Vec::new();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(opts.duration_secs);

    tracing::info!(duration_secs = opts.duration_secs, "recording...");

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Some(Ok(message))) => {
                let block = message
                    .state_msgs
                    .values()
                    .next()
                    .map(|state_msg| state_msg.header.number);
                tracing::debug!(
                    ?block,
                    protocol_count = message.state_msgs.len(),
                    "recorded message"
                );
                messages.push(message.into());
            }
            Ok(Some(Err(error))) => {
                // The synchronizer ends the stream after an error. The messages before it are a
                // complete prefix of the market, so they are kept.
                if messages.is_empty() {
                    anyhow::bail!("the Tycho stream failed before its first message: {error}");
                }
                tracing::warn!(
                    messages = messages.len(),
                    %error,
                    "the Tycho stream failed; keeping the messages recorded so far"
                );
                break;
            }
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

    stream_handle.abort();
    let actual_duration = start.elapsed().as_secs();
    tracing::info!(messages = messages.len(), actual_duration, "recording complete");

    let pools_toml = include_str!("../../../worker_pools.toml");
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
            schema_version: SCHEMA_VERSION,
        },
        tokens: all_tokens.into_values().collect(),
        messages,
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
