use std::time::{Duration, Instant};

use fynd_core::recording::{MarketRecording, RecordedUpdate, RecordingMetadata};
use tokio_stream::StreamExt;
use tycho_simulation::{
    evm::stream::ProtocolStreamBuilder,
    tycho_client::{
        feed::component_tracker::ComponentFilter,
        rpc::{HttpRPCClient, HttpRPCClientOptions, RPCClient},
    },
    tycho_common::{
        dto::{PaginationParams, ProtocolSystemsRequestBody},
        models::Chain,
    },
    tycho_core::traits::FeePriceGetter,
    tycho_ethereum::rpc::EthereumRpcClient,
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
}

/// Connect to Tycho, capture raw Update messages for the configured
/// duration, and return a MarketRecording.
pub async fn record_market(opts: &RecordingOptions) -> anyhow::Result<MarketRecording> {
    let chain = Chain::Ethereum;

    // 1. Resolve protocol list: use explicit list or discover from Tycho RPC
    let protocols = match &opts.protocols {
        Some(p) if !p.is_empty() => {
            tracing::info!(protocols = ?p, "using explicit protocol list");
            p.clone()
        }
        _ => {
            let discovered =
                fetch_protocol_systems(&opts.tycho_url, Some(&opts.tycho_api_key), chain).await?;
            tracing::info!(
                count = discovered.len(),
                ?discovered,
                "discovered protocols from Tycho RPC"
            );
            discovered
        }
    };

    // 2. Load tokens from Tycho (TLS enabled for production Tycho)
    let all_tokens = load_all_tokens(
        &opts.tycho_url,
        false, // use TLS
        Some(&opts.tycho_api_key),
        true,
        chain,
        Some(opts.min_token_quality),
        Some(opts.traded_n_days_ago),
    )
    .await?;
    tracing::info!(count = all_tokens.len(), "loaded tokens");

    // 3. Fetch gas price from RPC (if URL provided)
    let gas_price_wei = match &opts.rpc_url {
        Some(url) => match fetch_gas_price_wei(url).await {
            Ok(wei) => {
                tracing::info!(gas_price_wei = %wei, "captured gas price from RPC");
                Some(wei)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch gas price, using None");
                None
            }
        },
        None => {
            tracing::info!("no --rpc-url provided, gas price will not be recorded");
            None
        }
    };

    // 4. Build the protocol stream with TVL filtering
    // with_tvl_range(lower_bound, upper_bound): components are added when TVL >= upper
    // and removed when TVL < lower. Use same value for both (no hysteresis in recording).
    let tvl_filter = ComponentFilter::with_tvl_range(opts.min_tvl, opts.min_tvl);
    let builder = ProtocolStreamBuilder::new(&opts.tycho_url, chain);

    let builder =
        fynd_core::feed::protocol_registry::register_exchanges(builder, tvl_filter, &protocols)
            .map_err(|e| anyhow::anyhow!("failed to register exchanges: {e}"))?;

    let mut stream = builder
        .auth_key(Some(opts.tycho_api_key.clone()))
        .skip_state_decode_failures(true)
        .set_tokens(all_tokens)
        .await
        .build()
        .await?;

    // 5. Receive Update messages until duration expires
    let mut updates = Vec::new();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(opts.duration_secs);

    tracing::info!(duration_secs = opts.duration_secs, "recording stream updates...");

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
                updates.push(RecordedUpdate::from(update));
            }
            Ok(Some(Err(e))) => {
                tracing::warn!("stream error (continuing): {e}");
            }
            Ok(None) => {
                tracing::info!("stream ended before deadline");
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

    Ok(MarketRecording {
        metadata: RecordingMetadata {
            chain: "ethereum".to_string(),
            recorded_at_unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time went backwards")
                .as_secs(),
            fynd_version: env!("CARGO_PKG_VERSION").to_string(),
            recording_duration_s: actual_duration,
            num_updates: updates.len(),
            protocols: protocols.clone(),
            min_tvl: opts.min_tvl,
            min_token_quality: opts.min_token_quality,
            traded_n_days_ago: Some(opts.traded_n_days_ago),
            gas_price_wei,
        },
        updates,
    })
}

async fn fetch_gas_price_wei(rpc_url: &str) -> anyhow::Result<String> {
    use tycho_simulation::tycho_ethereum::gas::GasPrice;

    let client = EthereumRpcClient::new(rpc_url)
        .map_err(|e| anyhow::anyhow!("failed to create RPC client: {e}"))?;
    let block_gas_price = client
        .get_latest_fee_price()
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch gas price: {e}"))?;
    let gas_price_wei = match block_gas_price.pricing {
        GasPrice::Legacy { gas_price } => gas_price,
        // EIP-1559: use max_fee_per_gas as the upper bound
        other => {
            tracing::warn!(?other, "non-legacy gas price, falling back to 10 gwei");
            num_bigint::BigUint::from(10_000_000_000u64)
        }
    };
    Ok(gas_price_wei.to_string())
}

async fn fetch_protocol_systems(
    tycho_url: &str,
    auth_key: Option<&str>,
    chain: Chain,
) -> anyhow::Result<Vec<String>> {
    let rpc_url = format!("https://{tycho_url}");
    let rpc_options = HttpRPCClientOptions::new().with_auth_key(auth_key.map(|s| s.to_string()));
    let rpc_client = HttpRPCClient::new(&rpc_url, rpc_options)?;

    const PAGE_SIZE: i64 = 100;
    let mut all_protocols = Vec::new();
    let mut page = 0;

    loop {
        let request = ProtocolSystemsRequestBody {
            chain: chain.into(),
            pagination: PaginationParams { page, page_size: PAGE_SIZE },
        };
        let response = rpc_client
            .get_protocol_systems(&request)
            .await?;
        let count = response.protocol_systems.len();
        all_protocols.extend(response.protocol_systems);
        if (count as i64) < PAGE_SIZE {
            break;
        }
        page += 1;
    }

    Ok(all_protocols)
}
