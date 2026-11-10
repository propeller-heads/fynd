//! Starts the Tycho market and watches its feed.

use std::{sync::Arc, time::Duration};

use anyhow::{bail, Context as _};
use fynd_core::{FyndBuilder, MarketFeed};
use fynd_rpc::protocols::resolve_protocols;
use tokio::task::JoinHandle;
use tracing::info;
use tycho_simulation::{
    evm::tycho_models::Chain, tycho_common::models::chain_config::TvlThresholdTier,
    utils::default_blocklist,
};

use crate::market::market_args::MarketArgs;

/// How long the market may take to receive its first block and compute the derived data.
const MARKET_READY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How often the market feed is checked for having ended.
const FEED_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// A running market.
pub struct LaunchedMarket {
    /// The live market and its derived data, shared with whatever reads them.
    pub feed: Arc<MarketFeed>,
    /// Ends, with the reason, when the market did not become ready or the Tycho feed ended.
    pub task: JoinHandle<anyhow::Result<()>>,
}

/// Starts the market of `chain`: every on-chain protocol system Tycho serves, above the TVL
/// floor, with no worker pools.
///
/// # Errors
///
/// Fails when `--tycho-url` or `--rpc-url` is missing, when the protocol systems cannot be
/// fetched from Tycho, or when the feed cannot be built.
pub async fn launch_market(chain: Chain, args: &MarketArgs) -> anyhow::Result<LaunchedMarket> {
    let (Some(tycho_url), Some(rpc_url)) = (&args.tycho_url, &args.rpc_url) else {
        bail!("the market needs --tycho-url (TYCHO_URL) and --rpc-url (RPC_URL)");
    };
    let protocols =
        resolve_protocols(tycho_url, args.tycho_api_key.as_deref(), !args.disable_tls, chain, &[])
            .await
            .context("cannot resolve the protocol systems to track")?;
    info!(count = protocols.len(), "starting the market");
    let min_tvl = args
        .min_tvl
        .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Low));
    let mut builder =
        FyndBuilder::new(chain, tycho_url.clone(), rpc_url.clone(), protocols, min_tvl)
            .tycho_use_tls(!args.disable_tls)
            .blocklisted_components(default_blocklist());
    if let Some(api_key) = &args.tycho_api_key {
        builder = builder.tycho_api_key(api_key.clone());
    }
    let feed = Arc::new(
        builder
            .build_market()
            .context("cannot start the market")?,
    );
    let task = tokio::spawn(watch_market(Arc::clone(&feed)));
    Ok(LaunchedMarket { feed, task })
}

/// Waits until the market holds a block and its derived data is computed.
///
/// # Errors
///
/// Fails when that takes longer than 15 minutes.
pub async fn wait_until_market_ready(feed: &MarketFeed) -> anyhow::Result<()> {
    info!("waiting for the market to be ready");
    feed.wait_until_ready(MARKET_READY_TIMEOUT)
        .await
        .context("the market did not become ready")
}

/// Waits for the market to be ready, then returns an error once the Tycho feed has ended.
async fn watch_market(feed: Arc<MarketFeed>) -> anyhow::Result<()> {
    wait_until_market_ready(&feed).await?;
    info!("the market is ready");
    let mut feed_check = tokio::time::interval(FEED_CHECK_INTERVAL);
    loop {
        feed_check.tick().await;
        if feed.feed_has_ended() {
            bail!("the Tycho feed ended, so the market no longer updates");
        }
    }
}
