//! Chainlink on-chain price provider.
//!
//! Uses the Chainlink Feed Registry to dynamically query USD prices for any token
//! that has a registered feed. Prices are polled via RPC and cached in memory.
//! The [`ChainlinkProvider`] reads from this cache to validate solution prices.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::{
    primitives::{address, Address as AlloyAddress, U256},
    providers::ProviderBuilder,
    sol,
    transports::http::reqwest::Url,
};
use async_trait::async_trait;
use num_bigint::BigUint;
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};
use tycho_simulation::tycho_common::models::Address;

use super::{
    common::{compute_expected_out, normalize_symbol, resolve_token},
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
};
use crate::feed::market_data::SharedMarketData;

const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Chainlink feeds update on deviation (0.5% for majors, 0.25% for stablecoins)
/// or at a heartbeat interval (1h for majors, 24h for stablecoins).
/// We use a generous threshold to accommodate stablecoin heartbeats.
const STALENESS_THRESHOLD: Duration = Duration::from_secs(25 * 3600); // 25 hours

/// Chainlink Feed Registry on Ethereum mainnet.
const FEED_REGISTRY: AlloyAddress = address!("47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf");

/// USD denomination address in the Feed Registry (ISO 4217 code 840).
const USD_QUOTE: AlloyAddress = address!("0000000000000000000000000000000000000348");

/// Chainlink denomination address for ETH (used instead of WETH ERC-20 address).
const ETH_DENOMINATION: AlloyAddress = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

/// Chainlink denomination address for BTC (used instead of WBTC ERC-20 address).
const BTC_DENOMINATION: AlloyAddress = address!("bBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB");

sol! {
    #[sol(rpc)]
    interface IFeedRegistry {
        function latestRoundData(address base, address quote) external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
    }
}

/// Cached oracle price entry (USD-denominated).
#[derive(Debug, Clone)]
struct OraclePrice {
    usd_price: f64,
    timestamp_ms: u64,
}

/// Shared price cache. Key is the normalized asset symbol (e.g. "ETH").
type PriceCache = Arc<RwLock<HashMap<String, OraclePrice>>>;

/// Chainlink on-chain price provider.
///
/// All Chainlink USD feeds return prices with 8 decimals. We convert to f64 and
/// price any pair as `price_in_usd / price_out_usd`, same as Hyperliquid.
pub struct ChainlinkProvider {
    cache: PriceCache,
    /// Token registry for resolving on-chain addresses to exchange symbols and decimals.
    market_data: Arc<RwLock<SharedMarketData>>,
}

impl ChainlinkProvider {
    /// Starts the Chainlink price feed and returns a provider + background task handle.
    ///
    /// The background task polls the Chainlink Feed Registry via RPC for all tokens
    /// in market data and writes prices to a shared cache.
    pub fn start(
        rpc_url: String,
        market_data: Arc<RwLock<SharedMarketData>>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let cache: PriceCache = Arc::new(RwLock::new(HashMap::new()));
        let worker = ChainlinkWorker {
            cache: Arc::clone(&cache),
            rpc_url,
            market_data: Arc::clone(&market_data),
        };
        let handle = tokio::spawn(async move { worker.run().await });
        (Self { cache, market_data }, handle)
    }
}

#[async_trait]
impl PriceProvider for ChainlinkProvider {
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

        let oldest_ts = price_in.timestamp_ms.min(price_out.timestamp_ms);
        let age_ms = now_ms.saturating_sub(oldest_ts);
        if age_ms > STALENESS_THRESHOLD.as_millis() as u64 {
            return Err(PriceProviderError::StaleData { age_ms });
        }

        if price_out.usd_price == 0.0 {
            return Err(PriceProviderError::Unavailable("zero oracle price".into()));
        }

        let price = price_in.usd_price / price_out.usd_price;
        let expected_out = compute_expected_out(amount_in, price, dec_in, dec_out);

        Ok(ExternalPrice::new(expected_out, "chainlink".to_string(), oldest_ts))
    }
}

/// Background task that polls the Chainlink Feed Registry and populates the price cache.
struct ChainlinkWorker {
    cache: PriceCache,
    rpc_url: String,
    market_data: Arc<RwLock<SharedMarketData>>,
}

/// Maps a normalized token symbol to its Chainlink Feed Registry denomination address.
///
/// For ETH and BTC, the registry uses special denomination addresses (not ERC-20).
/// For all other tokens, the ERC-20 address works directly.
fn denomination_address(symbol: &str, erc20_address: &Address) -> AlloyAddress {
    match symbol {
        "ETH" => ETH_DENOMINATION,
        "BTC" => BTC_DENOMINATION,
        _ => tycho_to_alloy_address(erc20_address),
    }
}

/// Converts a tycho `Address` (Bytes) to an alloy `Address`.
fn tycho_to_alloy_address(addr: &Address) -> AlloyAddress {
    let bytes = addr.as_ref();
    if bytes.len() >= 20 {
        AlloyAddress::from_slice(&bytes[bytes.len() - 20..])
    } else {
        // Pad with leading zeros if shorter than 20 bytes
        let mut padded = [0u8; 20];
        padded[20 - bytes.len()..].copy_from_slice(bytes);
        AlloyAddress::from(padded)
    }
}

impl ChainlinkWorker {
    async fn run(&self) {
        loop {
            match self.poll().await {
                Ok(count) => debug!(count, "updated Chainlink oracle prices"),
                Err(e) => warn!(error = %e, "failed to poll Chainlink feeds"),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn poll(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let url: Url = self.rpc_url.parse()?;
        let provider = ProviderBuilder::new().connect_http(url);
        let registry = IFeedRegistry::new(FEED_REGISTRY, &provider);

        // Read all tokens from market data and build (symbol → denomination address) pairs.
        // Deduplicate by normalized symbol (e.g. WETH and ETH both map to "ETH").
        let tokens_to_query: Vec<(String, AlloyAddress)> = {
            let market_data = self.market_data.read().await;
            let mut seen = HashMap::new();
            for (addr, token) in market_data.token_registry_ref() {
                let symbol = normalize_symbol(&token.symbol).to_uppercase();
                seen.entry(symbol.clone())
                    .or_insert_with(|| denomination_address(&symbol, addr));
            }
            seen.into_iter().collect()
        };

        let mut cache = self.cache.write().await;
        let mut count = 0;

        for (symbol, base_addr) in &tokens_to_query {
            match registry.latestRoundData(*base_addr, USD_QUOTE).call().await {
                Ok(data) => {
                    // answer is int256 with 8 decimals for USD feeds.
                    let answer_i256 = data.answer;
                    if answer_i256.is_negative() {
                        continue;
                    }
                    let answer_u256: U256 = answer_i256.try_into().unwrap_or(U256::ZERO);
                    let usd_price = answer_u256.to::<u128>() as f64 / 1e8;

                    // updatedAt is Unix timestamp in seconds.
                    let updated_at_secs: u64 = data.updatedAt.to::<u64>();
                    let timestamp_ms = updated_at_secs * 1000;

                    if usd_price > 0.0 {
                        cache.insert(symbol.clone(), OraclePrice { usd_price, timestamp_ms });
                        count += 1;
                    }
                }
                Err(_) => {
                    // Feed doesn't exist for this token — skip silently.
                    trace!(symbol, "no Chainlink feed for token");
                }
            }
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::RwLock;
    use tycho_simulation::tycho_core::models::{token::Token, Chain};

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    #[test]
    fn test_denomination_addresses() {
        let weth_addr: Address = "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let usdc_addr: Address = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();

        // ETH uses special denomination address, not WETH ERC-20
        assert_eq!(denomination_address("ETH", &weth_addr), ETH_DENOMINATION);
        // USDC uses its ERC-20 address directly
        let usdc_alloy = denomination_address("USDC", &usdc_addr);
        assert_eq!(usdc_alloy, tycho_to_alloy_address(&usdc_addr));
    }

    #[test]
    fn test_tycho_to_alloy_address() {
        let addr: Address = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let alloy_addr = tycho_to_alloy_address(&addr);
        assert_eq!(
            format!("{:?}", alloy_addr).to_lowercase(),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    #[tokio::test]
    #[ignore] // requires RPC access
    async fn test_chainlink_provider_live() {
        // Integration test: starts the Chainlink provider, waits for its background
        // poller to populate the cache, then queries 1 WETH → USDC and checks that the
        // returned amount is in a sane range.
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

        let rpc_url =
            std::env::var("RPC_URL").expect("RPC_URL env var required for Chainlink live test");
        let (provider, handle) = ChainlinkProvider::start(rpc_url, market_data);

        // Chainlink polls every 10s; give it time to populate the cache.
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;

        let one_eth = BigUint::from(10u64).pow(18);
        let result = provider
            .get_expected_out(&weth_addr, &usdc_addr, &one_eth)
            .await;
        handle.abort();

        let price = result.expect("should get a price from Chainlink");
        let amount_out = price.expected_amount_out().clone();

        // 1 ETH should be worth between $100 and $100,000 USDC (6 decimals)
        let min = BigUint::from(100_000_000u64); // 100 USDC
        let max = BigUint::from(100_000_000_000u64); // 100,000 USDC
        assert!(
            amount_out >= min && amount_out <= max,
            "expected amount_out in [{min}, {max}], got {amount_out}"
        );
        println!("Chainlink: 1 WETH = {} USDC (raw)", amount_out);
    }
}
