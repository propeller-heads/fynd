//! Shared utilities for price providers.

use std::{sync::Arc, time::Duration};

use num_bigint::BigUint;
use tokio::sync::RwLock;
use tycho_simulation::tycho_common::models::{token::Token, Address};

use super::provider::PriceProviderError;
use crate::feed::market_data::SharedMarketData;

pub const STALENESS_THRESHOLD: Duration = Duration::from_secs(30);

/// Maps wrapped on-chain token symbols to their exchange equivalents.
pub fn normalize_symbol(symbol: &str) -> &str {
    match symbol.to_uppercase().as_str() {
        "WETH" => "ETH",
        "WBTC" => "BTC",
        "WBNB" => "BNB",
        "WMATIC" => "MATIC",
        "WAVAX" => "AVAX",
        _ => return symbol,
    }
}

/// Resolves an on-chain address to a (normalized_symbol, decimals) pair.
pub async fn resolve_token(
    market_data: &Arc<RwLock<SharedMarketData>>,
    address: &Address,
) -> Result<(String, u32), PriceProviderError> {
    let market_data = market_data.read().await;
    let token: &Token = market_data
        .get_token(address)
        .ok_or_else(|| PriceProviderError::PriceNotFound {
            token_in: format!("{:?}", address),
            token_out: "unknown".into(),
        })?;
    let symbol = normalize_symbol(&token.symbol).to_uppercase();
    Ok((symbol, token.decimals))
}

pub fn check_staleness(ticker_ts: u64, now_ms: u64) -> Result<(), PriceProviderError> {
    let age_ms = now_ms.saturating_sub(ticker_ts);
    if age_ms > STALENESS_THRESHOLD.as_millis() as u64 {
        return Err(PriceProviderError::StaleData { age_ms });
    }
    Ok(())
}

/// Computes the expected raw output amount given a price and token decimals.
///
/// `price` is in human terms: how many units of `token_out` per 1 unit of `token_in`.
/// `amount_in` is in raw units (e.g. wei). Returns raw units of `token_out`.
pub fn compute_expected_out(
    amount_in: &BigUint,
    price: f64,
    decimals_in: u32,
    decimals_out: u32,
) -> BigUint {
    const SCALE: f64 = 1_000_000_000_000.0; // 10^12

    let price_scaled = (price * SCALE) as u128;
    if price_scaled == 0 {
        return BigUint::ZERO;
    }

    let pow10 = |exp: u32| BigUint::from(10u64).pow(exp);
    let numerator = amount_in * BigUint::from(price_scaled) * pow10(decimals_out);
    let denominator = pow10(decimals_in) * BigUint::from(SCALE as u128);

    numerator / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_normalization() {
        assert_eq!(normalize_symbol("WETH"), "ETH");
        assert_eq!(normalize_symbol("WBTC"), "BTC");
        assert_eq!(normalize_symbol("WBNB"), "BNB");
        assert_eq!(normalize_symbol("USDC"), "USDC");
        assert_eq!(normalize_symbol("LINK"), "LINK");
        assert_eq!(normalize_symbol("weth"), "ETH");
    }

    #[test]
    fn test_compute_expected_out_eth_to_usdc() {
        // 1 ETH (18 decimals) at price 2000 USDC (6 decimals)
        let amount_in = BigUint::from(10u64).pow(18);
        let result = compute_expected_out(&amount_in, 2000.0, 18, 6);
        assert_eq!(result, BigUint::from(2_000_000_000u64));
    }

    #[test]
    fn test_compute_expected_out_usdc_to_eth() {
        // 2000 USDC (6 decimals) at price 0.0005 ETH (18 decimals)
        let amount_in = BigUint::from(2_000_000_000u64);
        let result = compute_expected_out(&amount_in, 0.0005, 6, 18);
        let one_eth = BigUint::from(10u64).pow(18);
        let diff = if result > one_eth { &result - &one_eth } else { &one_eth - &result };
        let tolerance = &one_eth / BigUint::from(1000u64); // 0.1%
        assert!(diff < tolerance, "result={result}, expected ~{one_eth}");
    }

    #[test]
    fn test_compute_expected_out_same_decimals() {
        // 100 USDC (6 dec) at price 1.0 to USDT (6 dec)
        let amount_in = BigUint::from(100_000_000u64);
        let result = compute_expected_out(&amount_in, 1.0, 6, 6);
        assert_eq!(result, BigUint::from(100_000_000u64));
    }

    #[test]
    fn test_staleness_detection() {
        let now_ms = 60_000u64;
        let old_ts = 1_000u64; // 59 seconds ago
        assert!(check_staleness(old_ts, now_ms).is_err());

        let fresh_ts = 50_000u64; // 10 seconds ago
        assert!(check_staleness(fresh_ts, now_ms).is_ok());
    }
}
