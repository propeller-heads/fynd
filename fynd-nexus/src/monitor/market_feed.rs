//! Starts the Tycho market feed that the monitor probes against.

use anyhow::{bail, Context as _};
use fynd_core::{FyndBuilder, MarketFeed};
use fynd_rpc::protocols::resolve_protocols;
use tracing::info;
use tycho_simulation::{
    evm::tycho_models::Chain, tycho_common::models::chain_config::TvlThresholdTier,
    utils::default_blocklist,
};

use crate::monitor::monitor_args::MonitorArgs;

/// Starts the market feed of `chain`: every on-chain protocol system Tycho serves, above the TVL
/// floor, with no worker pools.
///
/// # Errors
///
/// Fails when `--tycho-url` or `--rpc-url` is missing, when the protocol systems cannot be
/// fetched from Tycho, or when the feed cannot be built.
pub async fn start_market_feed(chain: Chain, args: &MonitorArgs) -> anyhow::Result<MarketFeed> {
    let (Some(tycho_url), Some(rpc_url)) = (&args.tycho_url, &args.rpc_url) else {
        bail!("the health monitor needs --tycho-url (TYCHO_URL) and --rpc-url (RPC_URL)");
    };
    let protocols =
        resolve_protocols(tycho_url, args.tycho_api_key.as_deref(), args.tycho_use_tls, chain, &[])
            .await
            .context("cannot resolve the protocol systems to track")?;
    info!(count = protocols.len(), "starting the market feed");
    let min_tvl = args
        .min_tvl
        .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Low));
    let mut builder =
        FyndBuilder::new(chain, tycho_url.clone(), rpc_url.clone(), protocols, min_tvl)
            .tycho_use_tls(args.tycho_use_tls)
            .blocklisted_components(default_blocklist());
    if let Some(api_key) = &args.tycho_api_key {
        builder = builder.tycho_api_key(api_key.clone());
    }
    builder
        .build_market()
        .context("cannot start the market feed")
}
