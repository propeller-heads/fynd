//! Hyperliquid oracle price provider.
//!
//! Polls the Hyperliquid REST API for oracle prices (weighted median across 8 CEXs) and caches them
//! in memory. The [`HyperliquidProvider`] reads from this cache to validate solution prices.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use num_bigint::BigUint;
use reqwest::Client;
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use tycho_simulation::tycho_common::models::{token::Token, Address};

use super::{
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
    utils::{check_staleness, expected_out_from_price},
};
use crate::feed::market_data::SharedMarketDataRef;

/// Maps on-chain token symbols to their Hyperliquid asset names.
///
/// Returns `(hyperliquid_symbol, price_scale)` where `price_scale` adjusts the oracle price
/// to a per-token basis. For most tokens this is `1.0`. Curated from the Hyperliquid perp
/// universe across three categories:
///
/// 1. **Wrapped native tokens** — on-chain "W"-prefixed wrappers of chain gas tokens. Tokens like
///    WLD/WIF whose names happen to start with W are NOT included.
/// 2. **k-prefix tokens** — Hyperliquid quotes these per 1,000 tokens to avoid tiny decimals.
///    `price_scale` is `0.001` so callers get the per-token price.
fn normalize_symbol(symbol: &str) -> (String, f64) {
    match symbol.to_uppercase().as_str() {
        // Wrapped native tokens
        "WETH" => ("ETH".to_string(), 1.0),
        "WBTC" => ("BTC".to_string(), 1.0),
        "WBNB" => ("BNB".to_string(), 1.0),
        "WMATIC" => ("MATIC".to_string(), 1.0),
        "WAVAX" => ("AVAX".to_string(), 1.0),
        "WFTM" => ("FTM".to_string(), 1.0),
        // k-prefix: Hyperliquid quotes per 1,000 tokens
        "PEPE" => ("kPEPE".to_string(), 0.001),
        "SHIB" => ("kSHIB".to_string(), 0.001),
        "BONK" => ("kBONK".to_string(), 0.001),
        "FLOKI" => ("kFLOKI".to_string(), 0.001),
        "LUNC" => ("kLUNC".to_string(), 0.001),
        "NEIRO" => ("kNEIRO".to_string(), 0.001),
        "DOGS" => ("kDOGS".to_string(), 0.001),
        other => (other.to_string(), 1.0),
    }
}

const API_URL: &str = "https://api.hyperliquid.xyz/info";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Stablecoins pegged to USD that aren't listed as Hyperliquid perps.
const USD_STABLECOINS: &[&str] = &["USDC", "USDT", "DAI", "FRAX"];

/// Cached oracle price entry (USD-denominated).
#[derive(Debug, Clone)]
struct OraclePrice {
    usd_price: f64,
    timestamp_ms: u64,
}

/// Shared price cache. Key is the Hyperliquid asset name (e.g. "ETH").
type PriceCache = Arc<RwLock<HashMap<String, OraclePrice>>>;

/// Cached token metadata resolved from on-chain addresses.
type TokenCache = Arc<RwLock<HashMap<Address, Token>>>;

/// Hyperliquid oracle price provider.
///
/// All oracle prices are in USD, so pricing any pair is `price_in / price_out`.
/// The background worker populates both the price cache (from the API) and a token
/// cache (snapshotted from `SharedMarketData`) so that `get_expected_out` never
/// touches the tokio `RwLock`.
pub struct HyperliquidProvider {
    price_cache: PriceCache,
    token_cache: TokenCache,
    poll_interval: Duration,
    api_url: String,
}

impl HyperliquidProvider {
    pub fn new(poll_interval: Duration) -> Self {
        Self {
            price_cache: Arc::new(RwLock::new(HashMap::new())),
            token_cache: Arc::new(RwLock::new(HashMap::new())),
            poll_interval,
            api_url: API_URL.to_string(),
        }
    }

    /// Resolves a token address to (hyperliquid_symbol, decimals, price_scale) from the local
    /// token cache. The `price_scale` factor converts Hyperliquid's oracle price to a per-token
    /// basis (relevant for k-prefix tokens).
    fn resolve_token(&self, address: &Address) -> Result<(String, u32, f64), PriceProviderError> {
        let cache = self
            .token_cache
            .read()
            .map_err(|e| PriceProviderError::Unavailable(format!("token cache poisoned: {e}")))?;
        let info = cache
            .get(address)
            .ok_or_else(|| PriceProviderError::TokenNotFound { address: address.to_string() })?;
        let (symbol, price_scale) = normalize_symbol(&info.symbol);
        Ok((symbol, info.decimals, price_scale))
    }
}

impl Default for HyperliquidProvider {
    fn default() -> Self {
        Self::new(DEFAULT_POLL_INTERVAL)
    }
}

impl PriceProvider for HyperliquidProvider {
    fn start(&mut self, market_data: SharedMarketDataRef) -> JoinHandle<()> {
        let worker = HyperliquidWorker {
            price_cache: Arc::clone(&self.price_cache),
            token_cache: Arc::clone(&self.token_cache),
            market_data,
            client: Client::new(),
            poll_interval: self.poll_interval,
            api_url: self.api_url.clone(),
        };
        tokio::spawn(async move { worker.run().await })
    }

    fn get_expected_out(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: &BigUint,
    ) -> Result<ExternalPrice, PriceProviderError> {
        let (sym_in, dec_in, scale_in) = self.resolve_token(token_in)?;
        let (sym_out, dec_out, scale_out) = self.resolve_token(token_out)?;

        let cache = self
            .price_cache
            .read()
            .map_err(|e| PriceProviderError::Unavailable(format!("price cache poisoned: {e}")))?;

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
        check_staleness(oldest_ts)?;

        if price_out.usd_price == 0.0 {
            return Err(PriceProviderError::Unavailable("zero oracle price".into()));
        }

        let usd_in = price_in.usd_price * scale_in;
        let usd_out = price_out.usd_price * scale_out;
        let price = usd_in / usd_out;
        let expected_out = expected_out_from_price(amount_in, price, dec_in, dec_out);

        Ok(ExternalPrice::new(expected_out, "hyperliquid".to_string(), oldest_ts))
    }
}

/// Background task that polls the Hyperliquid API and populates the price cache.
struct HyperliquidWorker {
    price_cache: PriceCache,
    token_cache: TokenCache,
    market_data: SharedMarketDataRef,
    client: Client,
    poll_interval: Duration,
    api_url: String,
}

impl HyperliquidWorker {
    async fn run(&self) {
        loop {
            self.refresh_token_cache().await;

            match self.poll_prices().await {
                Ok(count) => debug!(count, "updated Hyperliquid oracle prices"),
                Err(e) => warn!(error = %e, "failed to poll Hyperliquid oracle"),
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Snapshots the token registry from SharedMarketData into the local token cache.
    async fn refresh_token_cache(&self) {
        // Keep the `SharedMarketData` read-lock held for as short a time as possible:
        // snapshot only the fields we need, then build the local cache off-lock.
        let new_cache: HashMap<Address, Token> = {
            let data = self.market_data.read().await;
            data.token_registry_ref()
                .iter()
                .map(|(address, token)| (address.clone(), token.clone()))
                .collect()
        };

        let mut cache = match self.token_cache.write() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "token cache lock poisoned");
                return;
            }
        };

        *cache = new_cache;
    }

    async fn poll_prices(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let resp = self
            .client
            .post(&self.api_url)
            .json(&serde_json::json!({"type": "metaAndAssetCtxs"}))
            .send()
            .await?;

        let (meta, asset_ctxs): (Meta, Vec<AssetCtx>) = resp.json().await?;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Build the full snapshot before acquiring the write-lock to minimize hold time.
        let mut new_cache: HashMap<String, OraclePrice> = HashMap::new();
        let mut count = 0;

        for (asset, ctx) in meta
            .universe
            .iter()
            .zip(asset_ctxs.iter())
        {
            if let Ok(usd_price) = ctx.oracle_px.parse::<f64>() {
                if usd_price > 0.0 {
                    new_cache.insert(
                        asset.name.clone(),
                        OraclePrice { usd_price, timestamp_ms: now_ms },
                    );
                    count += 1;
                }
            }
        }

        for stable in USD_STABLECOINS {
            new_cache
                .entry((*stable).to_string())
                .or_insert(OraclePrice { usd_price: 1.0, timestamp_ms: now_ms });
        }

        let mut cache = self
            .price_cache
            .write()
            .map_err(|e| format!("price cache poisoned: {e}"))?;

        *cache = new_cache;

        Ok(count)
    }
}

// -- Hyperliquid API response types --
// Response is a JSON array: [meta, [asset_ctx, ...]]
// Deserializes as a tuple (Meta, Vec<AssetCtx>).

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
    use tokio::sync::RwLock;
    use tycho_simulation::{evm::tycho_models::Chain, tycho_common::models::token::Token};

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    fn make_token(address: Address, symbol: &str, decimals: u32) -> Token {
        Token {
            address,
            symbol: symbol.to_string(),
            decimals,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn weth_address() -> Address {
        "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .expect("valid address")
    }

    fn usdc_address() -> Address {
        "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .expect("valid address")
    }

    fn seeded_provider() -> HyperliquidProvider {
        let provider = HyperliquidProvider::default();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Seed price cache
        {
            let mut cache = provider
                .price_cache
                .write()
                .expect("lock");
            cache
                .insert("ETH".to_string(), OraclePrice { usd_price: 2000.0, timestamp_ms: now_ms });
            cache.insert("USDC".to_string(), OraclePrice { usd_price: 1.0, timestamp_ms: now_ms });
            cache.insert("LINK".to_string(), OraclePrice { usd_price: 15.0, timestamp_ms: now_ms });
            cache
                .insert("AAVE".to_string(), OraclePrice { usd_price: 200.0, timestamp_ms: now_ms });
        }

        // Seed token cache
        {
            let mut cache = provider
                .token_cache
                .write()
                .expect("lock");
            let weth = make_token(weth_address(), "WETH", 18);
            cache.insert(weth.address.clone(), weth);
            let usdc = make_token(usdc_address(), "USDC", 6);
            cache.insert(usdc.address.clone(), usdc);
            let link_addr: Address = "514910771AF9Ca656af840dff83E8264EcF986CA"
                .parse()
                .expect("valid address");
            let link = make_token(link_addr, "LINK", 18);
            cache.insert(link.address.clone(), link);
            let aave_addr: Address = "7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"
                .parse()
                .expect("valid address");
            let aave = make_token(aave_addr, "AAVE", 18);
            cache.insert(aave.address.clone(), aave);
        }

        provider
    }

    #[test]
    fn price_from_usd_oracle() {
        let provider = seeded_provider();
        let one_eth = BigUint::from(10u64).pow(18);

        let result = provider
            .get_expected_out(&weth_address(), &usdc_address(), &one_eth)
            .expect("should get price");

        // 1 ETH at $2000 → 2_000_000_000 USDC (6 decimals)
        assert_eq!(*result.expected_amount_out(), BigUint::from(2_000_000_000u64));
        assert_eq!(result.source(), "hyperliquid");
    }

    #[test]
    fn cross_pair_via_usd() {
        let provider = seeded_provider();
        let link_addr: Address = "514910771AF9Ca656af840dff83E8264EcF986CA"
            .parse()
            .expect("valid address");
        let aave_addr: Address = "7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"
            .parse()
            .expect("valid address");

        // 10 LINK ($15 each) → AAVE ($200 each) = 0.75 AAVE
        let ten_link = BigUint::from(10u64) * BigUint::from(10u64).pow(18);
        let result = provider
            .get_expected_out(&link_addr, &aave_addr, &ten_link)
            .expect("should get price");

        let expected = BigUint::from(75u64) * BigUint::from(10u64).pow(16); // 0.75 * 10^18
        let diff = if *result.expected_amount_out() > expected {
            result.expected_amount_out() - &expected
        } else {
            &expected - result.expected_amount_out()
        };
        let tolerance = &expected / BigUint::from(1000u64); // 0.1%
        assert!(diff < tolerance, "result={}, expected ~{expected}", result.expected_amount_out());
    }

    #[test]
    fn unknown_token_returns_error() {
        let provider = seeded_provider();
        let unknown: Address = "0000000000000000000000000000000000000001"
            .parse()
            .expect("valid");
        let one = BigUint::from(10u64).pow(18);

        let result = provider.get_expected_out(&unknown, &usdc_address(), &one);
        assert!(result.is_err());
        assert!(matches!(result, Err(PriceProviderError::TokenNotFound { .. })));
    }

    #[test]
    fn stale_price_returns_error() {
        let provider = HyperliquidProvider::default();
        let stale_ts = 1_000u64; // far in the past

        {
            let mut cache = provider
                .price_cache
                .write()
                .expect("lock");
            cache.insert(
                "ETH".to_string(),
                OraclePrice { usd_price: 2000.0, timestamp_ms: stale_ts },
            );
            cache
                .insert("USDC".to_string(), OraclePrice { usd_price: 1.0, timestamp_ms: stale_ts });
        }
        {
            let mut cache = provider
                .token_cache
                .write()
                .expect("lock");
            let weth = make_token(weth_address(), "WETH", 18);
            cache.insert(weth.address.clone(), weth);
            let usdc = make_token(usdc_address(), "USDC", 6);
            cache.insert(usdc.address.clone(), usdc);
        }

        let one_eth = BigUint::from(10u64).pow(18);
        let result = provider.get_expected_out(&weth_address(), &usdc_address(), &one_eth);
        assert!(result.is_err());
        assert!(matches!(result, Err(PriceProviderError::StaleData { .. })));
    }

    #[test]
    fn parse_meta_and_asset_ctxs() {
        let json = r#"[
            {"universe": [{"name": "BTC", "szDecimals": 5}, {"name": "ETH", "szDecimals": 4}]},
            [{"oraclePx": "66820.0", "markPx": "66787.0"}, {"oraclePx": "1989.0", "markPx": "1988.0"}]
        ]"#;

        let (meta, ctxs): (Meta, Vec<AssetCtx>) = serde_json::from_str(json).expect("should parse");
        assert_eq!(meta.universe.len(), 2);
        assert_eq!(meta.universe[0].name, "BTC");
        assert_eq!(meta.universe[1].name, "ETH");
        assert_eq!(ctxs[0].oracle_px, "66820.0");
        assert_eq!(ctxs[1].oracle_px, "1989.0");
    }

    #[tokio::test]
    #[ignore] // requires network access
    async fn hyperliquid_live_pepe_usdc() {
        // Tests functionality using an asset prefixed with k: meaning the price is for
        // 1000 PEPE instead of a single PEPE
        let pepe_addr: Address = "6982508145454Ce325dDbE47a25d4ec3d2311933"
            .parse()
            .expect("valid address");
        let pepe = make_token(pepe_addr.clone(), "PEPE", 18);
        let usdc = make_token(usdc_address(), "USDC", 6);

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([pepe, usdc]);
        let market_data = Arc::new(RwLock::new(market_data));

        let mut provider = HyperliquidProvider::default();
        let _handle = provider.start(market_data);

        tokio::time::sleep(Duration::from_secs(5)).await;

        // 1 billion PEPE → USDC
        let one_billion_pepe = BigUint::from(10u64).pow(27);
        let price = provider
            .get_expected_out(&pepe_addr, &usdc_address(), &one_billion_pepe)
            .expect("should get a price from Hyperliquid for PEPE");

        // 1B PEPE should be worth between $1,000 and $100,000 USDC (6 decimals)
        let min = BigUint::from(1_000_000_000u64); // $1,000
        let max = BigUint::from(100_000_000_000u64); // $100,000
        let amount = price.expected_amount_out();
        assert!(
            *amount >= min && *amount <= max,
            "expected 1B PEPE worth in [{min}, {max}] USDC, got {amount}"
        );
    }

    #[tokio::test]
    #[ignore] // requires network access
    async fn hyperliquid_live_weth_usdc() {
        let weth = make_token(weth_address(), "WETH", 18);
        let usdc = make_token(usdc_address(), "USDC", 6);

        let mut market_data = SharedMarketData::new();
        market_data.upsert_tokens([weth, usdc]);
        let market_data = Arc::new(RwLock::new(market_data));

        let mut provider = HyperliquidProvider::default();
        let _handle = provider.start(market_data);

        // Wait for the first poll cycle to populate the cache.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let one_eth = BigUint::from(10u64).pow(18);
        let price = provider
            .get_expected_out(&weth_address(), &usdc_address(), &one_eth)
            .expect("should get a price from Hyperliquid");

        // 1 ETH should be worth between $100 and $10,000 USDC (6 decimals)
        let min = BigUint::from(1_000_000_000u64); // $1,000
        let max = BigUint::from(10_000_000_000u64); // $10,000
        let amount = price.expected_amount_out();
        assert!(
            *amount >= min && *amount <= max,
            "expected amount_out in [{min}, {max}], got {amount}"
        );
    }
}
