use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;
use tracing::{debug, warn};

const API_BASE: &str = "https://api.coingecko.com/api/v3";

/// Hardcoded stablecoin addresses with approximate market caps (USD).
const STABLECOINS: &[(&str, f64)] = &[
    ("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", 43_000_000_000.0), // USDC
    ("0xdac17f958d2ee523a2206206994597c13d831ec7", 140_000_000_000.0), // USDT
    ("0x6b175474e89094c44da98b954eedeac495271d0f", 5_000_000_000.0), // DAI
    ("0x853d955acef822db058eb8505911ed77f175b99e", 600_000_000.0), // FRAX
    ("0x4fabb145d64652a948d72533023f6e7a623c7c53", 100_000_000.0), // BUSD
    ("0x0000000000085d4780b73119b644ae5ecd22b376", 100_000_000.0), // TUSD
    ("0x8e870d67f660d95d5be530380d0ec0bd388289e1", 800_000_000.0), // USDP
    ("0x1a7e4e63778b4f12a199c062f3efdd288afcbce8", 100_000_000.0), // agEUR
    ("0x5f98805a4e8be255a32880fdec7f6728c6568ba0", 300_000_000.0), // LUSD
    ("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", 300_000_000_000.0), // WETH (not stable but always known)
];

/// Thresholds for market cap classification (USD).
const BLUE_CHIP_THRESHOLD: f64 = 10_000_000_000.0; // $10B
const MID_CAP_THRESHOLD: f64 = 100_000_000.0; // $100M
const LONG_TAIL_THRESHOLD: f64 = 1_000_000.0; // $1M

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenCategory {
    Stable,
    BlueChip,
    MidCap,
    LongTail,
    Meme,
}

impl TokenCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::BlueChip => "blue_chip",
            Self::MidCap => "mid_cap",
            Self::LongTail => "long_tail",
            Self::Meme => "meme",
        }
    }
}

impl std::fmt::Display for TokenCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub market_cap: Option<f64>,
    pub fdv: Option<f64>,
    pub category: TokenCategory,
}

#[derive(Debug, Clone)]
pub struct PairClassification {
    pub bucket: String,
    pub log_mcap_ratio: Option<f64>,
    pub min_mcap: Option<f64>,
    pub max_mcap: Option<f64>,
}

/// CoinGecko API response (subset of fields we need).
#[derive(Debug, Deserialize)]
struct CoinGeckoResponse {
    market_data: Option<MarketDataResponse>,
}

#[derive(Debug, Deserialize)]
struct MarketDataResponse {
    market_cap: Option<CurrencyValue>,
    fully_diluted_valuation: Option<CurrencyValue>,
}

#[derive(Debug, Deserialize)]
struct CurrencyValue {
    usd: Option<f64>,
}

pub struct CoinGeckoClient {
    api_key: String,
    cache: HashMap<String, TokenMetadata>,
    http_client: reqwest::Client,
}

impl CoinGeckoClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key, cache: HashMap::new(), http_client: reqwest::Client::new() }
    }

    /// Look up a token by Ethereum contract address.
    ///
    /// Returns cached data if available, otherwise fetches from the API.
    pub async fn get_token_metadata(&mut self, address: &str) -> anyhow::Result<&TokenMetadata> {
        let normalized = address.to_lowercase();

        if self.cache.contains_key(&normalized) {
            return Ok(self
                .cache
                .get(&normalized)
                .expect("just checked contains_key"));
        }

        let metadata = self
            .fetch_token_metadata(&normalized)
            .await?;
        self.cache
            .insert(normalized.clone(), metadata);
        Ok(self
            .cache
            .get(&normalized)
            .expect("just inserted"))
    }

    async fn fetch_token_metadata(&self, address: &str) -> anyhow::Result<TokenMetadata> {
        if let Some((_, mcap)) = STABLECOINS.iter().find(|(addr, _)| *addr == address) {
            let category = if address == "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" {
                TokenCategory::BlueChip
            } else {
                TokenCategory::Stable
            };
            return Ok(TokenMetadata {
                market_cap: Some(*mcap),
                fdv: Some(*mcap),
                category,
            });
        }

        let url = format!("{}/coins/ethereum/contract/{}", API_BASE, address);

        debug!(address, url = %url, "fetching CoinGecko token metadata");

        let response = self
            .http_client
            .get(&url)
            .header("x-cg-demo-api-key", &self.api_key)
            .header("accept", "application/json")
            .send()
            .await
            .context("CoinGecko API request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_default();
            warn!(
                address,
                status = %status,
                body = %body,
                "CoinGecko API returned non-200, defaulting to long_tail"
            );
            return Ok(TokenMetadata {
                market_cap: None,
                fdv: None,
                category: TokenCategory::LongTail,
            });
        }

        let data: CoinGeckoResponse = response
            .json()
            .await
            .context("failed to parse CoinGecko response")?;

        let market_cap = data
            .market_data
            .as_ref()
            .and_then(|md| md.market_cap.as_ref())
            .and_then(|mc| mc.usd);

        let fdv = data
            .market_data
            .as_ref()
            .and_then(|md| md.fully_diluted_valuation.as_ref())
            .and_then(|v| v.usd);

        let category = classify_token(market_cap);

        debug!(
            address,
            market_cap = ?market_cap,
            fdv = ?fdv,
            category = %category,
            "classified token"
        );

        Ok(TokenMetadata { market_cap, fdv, category })
    }

    /// Classify a token pair and compute continuous features.
    pub fn classify_pair(
        &self,
        token_a: &TokenMetadata,
        token_b: &TokenMetadata,
    ) -> PairClassification {
        let bucket = pair_bucket(&token_a.category, &token_b.category);

        let (log_mcap_ratio, min_mcap, max_mcap) = match (token_a.market_cap, token_b.market_cap) {
            (Some(a), Some(b)) if a > 0.0 && b > 0.0 => {
                let max = a.max(b);
                let min = a.min(b);
                let ratio = (max / min).ln();
                (Some(ratio), Some(min), Some(max))
            }
            _ => (None, None, None),
        };

        PairClassification { bucket, log_mcap_ratio, min_mcap, max_mcap }
    }
}

fn classify_token(market_cap: Option<f64>) -> TokenCategory {
    let Some(mcap) = market_cap else {
        return TokenCategory::LongTail;
    };

    if mcap >= BLUE_CHIP_THRESHOLD {
        TokenCategory::BlueChip
    } else if mcap >= MID_CAP_THRESHOLD {
        TokenCategory::MidCap
    } else if mcap >= LONG_TAIL_THRESHOLD {
        TokenCategory::LongTail
    } else {
        TokenCategory::Meme
    }
}

/// Classify a pair into a bucket based on two token categories.
///
/// Bucket names are alphabetically ordered to ensure that
/// (stable, large) and (large, stable) both produce "stable-large".
fn pair_bucket(cat_a: &TokenCategory, cat_b: &TokenCategory) -> String {
    use TokenCategory::{BlueChip, LongTail, Meme, MidCap, Stable};

    match (cat_a, cat_b) {
        (Stable, Stable) => "stable-stable".to_string(),
        (Stable, BlueChip) | (BlueChip, Stable) => "stable-large".to_string(),
        (BlueChip, BlueChip) => "large-large".to_string(),
        (Stable, MidCap) | (MidCap, Stable) => "stable-mid".to_string(),
        (Stable, Meme) | (Meme, Stable) => "stable-meme".to_string(),
        (Stable, LongTail) | (LongTail, Stable) => "stable-longtail".to_string(),
        (BlueChip, MidCap) | (MidCap, BlueChip) => "large-mid".to_string(),
        (BlueChip, LongTail) | (LongTail, BlueChip) => "large-longtail".to_string(),
        (BlueChip, Meme) | (Meme, BlueChip) => "large-meme".to_string(),
        (MidCap, MidCap) => "mid-mid".to_string(),
        (MidCap, LongTail) | (LongTail, MidCap) => "mid-longtail".to_string(),
        (MidCap, Meme) | (Meme, MidCap) => "mid-meme".to_string(),
        (LongTail, LongTail) => "longtail-longtail".to_string(),
        (LongTail, Meme) | (Meme, LongTail) => "longtail-meme".to_string(),
        (Meme, Meme) => "meme-meme".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_stablecoin_by_address() {
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        assert!(STABLECOINS.iter().any(|(addr, _)| *addr == usdc));
    }

    #[test]
    fn classify_token_by_mcap() {
        assert_eq!(classify_token(Some(50_000_000_000.0)), TokenCategory::BlueChip);
        assert_eq!(classify_token(Some(500_000_000.0)), TokenCategory::MidCap);
        assert_eq!(classify_token(Some(5_000_000.0)), TokenCategory::LongTail);
        assert_eq!(classify_token(Some(500_000.0)), TokenCategory::Meme);
        assert_eq!(classify_token(None), TokenCategory::LongTail);
    }

    #[test]
    fn classify_token_at_boundaries() {
        assert_eq!(classify_token(Some(BLUE_CHIP_THRESHOLD)), TokenCategory::BlueChip);
        assert_eq!(classify_token(Some(MID_CAP_THRESHOLD)), TokenCategory::MidCap);
        assert_eq!(classify_token(Some(LONG_TAIL_THRESHOLD)), TokenCategory::LongTail);
        assert_eq!(classify_token(Some(LONG_TAIL_THRESHOLD - 1.0)), TokenCategory::Meme);
    }

    #[test]
    fn pair_bucket_symmetric() {
        assert_eq!(pair_bucket(&TokenCategory::Stable, &TokenCategory::BlueChip), "stable-large");
        assert_eq!(pair_bucket(&TokenCategory::BlueChip, &TokenCategory::Stable), "stable-large");
    }

    #[test]
    fn pair_bucket_stable_stable() {
        assert_eq!(pair_bucket(&TokenCategory::Stable, &TokenCategory::Stable), "stable-stable");
    }

    #[test]
    fn pair_bucket_meme_stable() {
        assert_eq!(pair_bucket(&TokenCategory::Meme, &TokenCategory::Stable), "stable-meme");
    }

    #[test]
    fn classify_pair_with_mcaps() {
        let client = CoinGeckoClient::new("test".to_string());
        let token_a = TokenMetadata {
            market_cap: Some(1_000_000_000.0),
            fdv: None,
            category: TokenCategory::MidCap,
        };
        let token_b = TokenMetadata {
            market_cap: Some(100_000_000.0),
            fdv: None,
            category: TokenCategory::MidCap,
        };

        let pair = client.classify_pair(&token_a, &token_b);
        assert_eq!(pair.bucket, "mid-mid");
        assert!(pair.log_mcap_ratio.is_some());
        let ratio = pair.log_mcap_ratio.expect("has ratio");
        // ln(1_000_000_000 / 100_000_000) = ln(10) ~ 2.302
        assert!((ratio - 10.0_f64.ln()).abs() < 0.01);
        assert_eq!(pair.min_mcap, Some(100_000_000.0));
        assert_eq!(pair.max_mcap, Some(1_000_000_000.0));
    }

    #[test]
    fn classify_pair_without_mcaps() {
        let client = CoinGeckoClient::new("test".to_string());
        let token_a =
            TokenMetadata { market_cap: None, fdv: None, category: TokenCategory::LongTail };
        let token_b = TokenMetadata {
            market_cap: Some(100_000_000.0),
            fdv: None,
            category: TokenCategory::MidCap,
        };

        let pair = client.classify_pair(&token_a, &token_b);
        assert_eq!(pair.bucket, "mid-longtail");
        assert!(pair.log_mcap_ratio.is_none());
        assert!(pair.min_mcap.is_none());
        assert!(pair.max_mcap.is_none());
    }

    #[test]
    fn token_category_display() {
        assert_eq!(TokenCategory::Stable.as_str(), "stable");
        assert_eq!(TokenCategory::BlueChip.as_str(), "blue_chip");
        assert_eq!(TokenCategory::MidCap.as_str(), "mid_cap");
        assert_eq!(TokenCategory::LongTail.as_str(), "long_tail");
        assert_eq!(TokenCategory::Meme.as_str(), "meme");
    }

    #[test]
    fn all_pair_buckets_covered() {
        use TokenCategory::{BlueChip, LongTail, Meme, MidCap, Stable};
        let categories = [Stable, BlueChip, MidCap, LongTail, Meme];

        for a in &categories {
            for b in &categories {
                let bucket = pair_bucket(a, b);
                assert!(!bucket.is_empty(), "missing bucket for {:?} x {:?}", a, b);
            }
        }
    }
}
