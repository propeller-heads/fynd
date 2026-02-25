//! Hyperliquid oracle price provider.
//!
//! Polls the Hyperliquid REST API for oracle prices (weighted median across 8 CEXs)
//! and caches them in memory. The [`HyperliquidProvider`] reads from this cache to
//! validate solution prices.

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use num_bigint::BigUint;
use reqwest::Client;
use tokio::sync::RwLock;
use tracing::{debug, warn};
use tycho_simulation::tycho_common::models::Address;

use super::{
    common::{check_staleness, compute_expected_out, resolve_token},
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
};
use crate::feed::market_data::SharedMarketData;

const API_URL: &str = "https://api.hyperliquid.xyz/info";
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Cached oracle price entry (USD-denominated).
#[derive(Debug, Clone)]
struct OraclePrice {
    usd_price: f64,
    timestamp_ms: u64,
}

/// Shared price cache. Key is the Hyperliquid asset name (e.g. "ETH").
///
/// We cache prices rather than fetching on demand because the `metaAndAssetCtxs` endpoint
/// has a weight of 20 against a 1200/min rate limit (~60 calls/min). Polling every 3s in the
/// background stays well within limits regardless of solve request volume.
type PriceCache = Arc<RwLock<HashMap<String, OraclePrice>>>;

/// Hyperliquid oracle price provider.
///
/// All oracle prices are in USD, so pricing any pair is just `price_in / price_out`.
pub struct HyperliquidProvider {
    cache: PriceCache,
    market_data: Arc<RwLock<SharedMarketData>>,
}

impl HyperliquidProvider {
    /// Starts the Hyperliquid price feed and returns a provider + background task handle.
    ///
    /// The background task polls the Hyperliquid API for oracle prices and writes them
    /// to a shared cache. The returned provider reads from that cache.
    pub fn start(
        market_data: Arc<RwLock<SharedMarketData>>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let cache: PriceCache = Arc::new(RwLock::new(HashMap::new()));
        let worker = HyperliquidWorker { cache: Arc::clone(&cache), client: Client::new() };
        let handle = tokio::spawn(async move { worker.run().await });
        (Self { cache, market_data }, handle)
    }
}

#[async_trait]
impl PriceProvider for HyperliquidProvider {
    async fn get_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Result<ExternalPrice, PriceProviderError> {
        let (sym_in, dec_in) = resolve_token(&self.market_data, token_in).await?;
        let (sym_out, dec_out) = resolve_token(&self.market_data, token_out).await?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let cache = self.cache.read().await;

        let price_in = cache
            .get(&sym_in)
            .ok_or_else(|| PriceProviderError::PriceNotFound {
                token_in: sym_in.clone(),
                token_out: sym_out.clone(),
            })?;
        let price_out = cache
            .get(&sym_out)
            .ok_or_else(|| PriceProviderError::PriceNotFound {
                token_in: sym_in.clone(),
                token_out: sym_out.clone(),
            })?;

        let oldest_ts = price_in
            .timestamp_ms
            .min(price_out.timestamp_ms);
        check_staleness(oldest_ts, now_ms)?;

        if price_out.usd_price == 0.0 {
            return Err(PriceProviderError::Unavailable("zero oracle price".into()));
        }

        let price = price_in.usd_price / price_out.usd_price;
        let expected_out = compute_expected_out(amount_in, price, dec_in, dec_out);

        Ok(ExternalPrice::new(expected_out, "hyperliquid".to_string(), oldest_ts))
    }
}

/// Background task that polls the Hyperliquid API and populates the price cache.
struct HyperliquidWorker {
    cache: PriceCache,
    client: Client,
}

impl HyperliquidWorker {
    async fn run(&self) {
        loop {
            match self.poll().await {
                Ok(count) => debug!(count, "updated Hyperliquid oracle prices"),
                Err(e) => warn!(error = %e, "failed to poll Hyperliquid oracle"),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn poll(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let resp = self
            .client
            .post(API_URL)
            .json(&serde_json::json!({"type": "metaAndAssetCtxs"}))
            .send()
            .await?;

        let body: MetaAndAssetCtxsResponse = resp.json().await?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut cache = self.cache.write().await;
        let mut count = 0;

        for (meta, ctx) in body
            .meta
            .universe
            .iter()
            .zip(body.asset_ctxs.iter())
        {
            if let Ok(usd_price) = ctx.oracle_px.parse::<f64>() {
                if usd_price > 0.0 {
                    cache
                        .insert(meta.name.clone(), OraclePrice { usd_price, timestamp_ms: now_ms });
                    count += 1;
                }
            }
        }

        // Stablecoins pegged to USD aren't listed as perps but we need them for pricing.
        // Insert them at $1.00 so USDC, USDT, DAI etc. resolve correctly.
        for stable in &["USDC", "USDT", "DAI", "FRAX"] {
            cache
                .entry(stable.to_string())
                .or_insert(OraclePrice { usd_price: 1.0, timestamp_ms: now_ms });
        }

        Ok(count)
    }
}

/// Response from `metaAndAssetCtxs` — a two-element JSON array `[meta, [ctx, ...]]`.
#[derive(serde::Deserialize)]
struct MetaAndAssetCtxsResponse {
    #[serde(rename = "0")]
    meta: Meta,
    #[serde(rename = "1")]
    asset_ctxs: Vec<AssetCtx>,
}

/// Metadata containing the asset universe.
#[derive(serde::Deserialize)]
struct Meta {
    universe: Vec<AssetMeta>,
}

/// Per-asset metadata.
#[derive(serde::Deserialize)]
struct AssetMeta {
    name: String,
}

/// Per-asset context containing oracle price.
#[derive(serde::Deserialize)]
struct AssetCtx {
    #[serde(rename = "oraclePx")]
    oracle_px: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::RwLock;
    use tycho_simulation::tycho_core::models::{token::Token, Chain};

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    #[test]
    fn test_price_from_usd_oracle() {
        // ETH at $2000, USDC at $1 → price = 2000/1 = 2000
        let price = 2000.0_f64 / 1.0_f64;
        let amount_in = BigUint::from(10u64).pow(18); // 1 ETH
        let result = compute_expected_out(&amount_in, price, 18, 6);
        assert_eq!(result, BigUint::from(2_000_000_000u64));
    }

    #[test]
    fn test_cross_pair_via_usd() {
        // LINK at $15, AAVE at $200 → LINK/AAVE price = 15/200 = 0.075
        let price = 15.0_f64 / 200.0_f64;
        // 10 LINK (18 decimals) → should get 0.75 AAVE (18 decimals)
        let amount_in = BigUint::from(10u64) * BigUint::from(10u64).pow(18);
        let result = compute_expected_out(&amount_in, price, 18, 18);
        let expected = BigUint::from(75u64) * BigUint::from(10u64).pow(16); // 0.75 * 10^18
        let diff = if result > expected { &result - &expected } else { &expected - &result };
        let tolerance = &expected / BigUint::from(1000u64); // 0.1%
        assert!(diff < tolerance, "result={result}, expected ~{expected}");
    }

    #[test]
    fn test_parse_meta_and_asset_ctxs() {
        let json = r#"[
            {"universe": [{"name": "BTC", "szDecimals": 5}, {"name": "ETH", "szDecimals": 4}]},
            [{"oraclePx": "66820.0", "markPx": "66787.0"}, {"oraclePx": "1989.0", "markPx": "1988.0"}]
        ]"#;

        let resp: MetaAndAssetCtxsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.meta.universe.len(), 2);
        assert_eq!(resp.meta.universe[0].name, "BTC");
        assert_eq!(resp.meta.universe[1].name, "ETH");
        assert_eq!(resp.asset_ctxs[0].oracle_px, "66820.0");
        assert_eq!(resp.asset_ctxs[1].oracle_px, "1989.0");
    }

    #[tokio::test]
    #[ignore] // requires network access
    async fn test_hyperliquid_provider_live() {
        /// Integration test: starts the Hyperliquid provider, waits for its background
        /// poller to populate the cache, then queries 1 WETH → USDC and checks that the
        /// returned amount is in a sane range.
        let weth_addr: Address = "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let usdc_addr: Address = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();

        let weth = Token {
            address: weth_addr.clone(),
            symbol: "WETH".to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };
        let usdc = Token {
            address: usdc_addr.clone(),
            symbol: "USDC".to_string(),
            decimals: 6,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        };

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([weth, usdc]);
        let market_data = Arc::new(RwLock::new(market_data));

        let (provider, handle) = HyperliquidProvider::start(market_data);

        // Hyperliquid polls every 3s; give it time to populate the cache.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let one_eth = BigUint::from(10u64).pow(18);
        let result = provider
            .get_expected_out(&weth_addr, &usdc_addr, &one_eth)
            .await;
        handle.abort();

        let price = result.expect("should get a price from Hyperliquid");
        let amount_out = price.expected_amount_out().clone();

        // 1 ETH should be worth between $100 and $100,000 USDC (6 decimals)
        let min = BigUint::from(100_000_000u64); // 100 USDC
        let max = BigUint::from(100_000_000_000u64); // 100,000 USDC
        assert!(
            amount_out >= min && amount_out <= max,
            "expected amount_out in [{min}, {max}], got {amount_out}"
        );
        println!("Hyperliquid: 1 WETH = {} USDC (raw)", amount_out);
    }
}
