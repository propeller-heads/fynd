//! Helper functions for PriceGuard calculations.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Address;

use super::provider::PriceProviderError;
use crate::feed::market_data::SharedMarketDataRef;

/// Maximum age of price data before it is considered stale.
pub const STALENESS_THRESHOLD: Duration = Duration::from_secs(30);

/// Maps wrapped on-chain token symbols to their offchain exchange equivalents.
///
/// On-chain tokens like WETH, WBTC are listed as ETH, BTC on offchain exchanges.
pub fn normalize_symbol(symbol: &str) -> String {
    match symbol.to_uppercase().as_str() {
        "WETH" => "ETH".to_string(),
        "WBTC" => "BTC".to_string(),
        "WBNB" => "BNB".to_string(),
        "WMATIC" => "MATIC".to_string(),
        "WAVAX" => "AVAX".to_string(),
        _ => symbol.to_uppercase(),
    }
}

/// Resolves an on-chain address to a (symbol, decimals) pair via the
/// [`SharedMarketData`](crate::feed::market_data::SharedMarketData) token registry.
pub async fn resolve_token(
    market_data: &SharedMarketDataRef,
    address: &Address,
) -> Result<(String, u32), PriceProviderError> {
    let data = market_data.read().await;
    let token = data
        .get_token(address)
        .ok_or_else(|| PriceProviderError::TokenNotFound { address: address.to_string() })?;
    Ok((token.symbol.clone(), token.decimals))
}

/// Returns `Err(StaleData)` if `ticker_ts` is older than [`STALENESS_THRESHOLD`].
pub fn check_staleness(ticker_ts: u64) -> Result<(), PriceProviderError> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let age_ms = now_ms.saturating_sub(ticker_ts);
    if age_ms > STALENESS_THRESHOLD.as_millis() as u64 {
        return Err(PriceProviderError::StaleData { age_ms });
    }
    Ok(())
}

/// Converts a human-readable price into a raw token amount.
///
/// Given `amount_in` in atomic units (e.g. wei) and a `price` expressing how many
/// units of `token_out` one unit of `token_in` buys, returns the expected atomic
/// output amount adjusted for the decimal difference between the two tokens.
///
/// # Example
///
/// 1 ETH at 2000 USDC/ETH:
/// - `amount_in = 10^18`, `price = 2000.0`
/// - `decimals_in = 18` (ETH), `decimals_out = 6` (USDC)
/// - Returns `2_000_000_000` (2000 USDC in atomic units)
pub fn expected_out_from_price(
    amount_in: &BigUint,
    price: f64,
    decimals_in: u32,
    decimals_out: u32,
) -> BigUint {
    // To avoid precision loss, we scale the price to an integer.
    const PRECISION: f64 = 1_000_000_000_000_000_000.0; // 10^18

    let price_scaled = (price * PRECISION) as u128;
    if price_scaled == 0 {
        return BigUint::ZERO;
    }

    let numerator =
        amount_in * BigUint::from(price_scaled) * BigUint::from(10u64).pow(decimals_out);
    let denominator = BigUint::from(10u64).pow(decimals_in) * BigUint::from(PRECISION as u128);

    numerator / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_out_eth_to_usdc() {
        // 1 ETH (18 decimals) at price 2000 USDC (6 decimals)
        let amount_in = BigUint::from(10u64).pow(18);
        let result = expected_out_from_price(&amount_in, 2000.0, 18, 6);
        assert_eq!(result, BigUint::from(2_000_000_000u64));
    }

    #[test]
    fn expected_out_usdc_to_eth() {
        // 2000 USDC (6 decimals) at price 0.0005 ETH (18 decimals)
        let amount_in = BigUint::from(2_000_000_000u64);
        let result = expected_out_from_price(&amount_in, 0.0005, 6, 18);
        let one_eth = BigUint::from(10u64).pow(18);
        let diff = if result > one_eth { &result - &one_eth } else { &one_eth - &result };
        let tolerance = &one_eth / BigUint::from(1000u64); // 0.1%
        assert!(diff < tolerance, "result={result}, expected ~{one_eth}");
    }
    #[test]
    fn expected_out_zero_price() {
        let amount_in = BigUint::from(10u64).pow(18);
        let result = expected_out_from_price(&amount_in, 0.0, 18, 6);
        assert_eq!(result, BigUint::ZERO);
    }

    #[test]
    fn expected_out_micro_price() {
        // 1 billion PEPE (18 decimals) at price 8×10^-11 BTC (8 decimals)
        // Expected: 1e9 × 8e-11 = 0.08 BTC = 8_000_000 satoshis
        let amount_in = BigUint::from(10u64).pow(27); // 1e9 PEPE in raw
        let result = expected_out_from_price(&amount_in, 8e-11, 18, 8);
        assert_eq!(result, BigUint::from(8_000_000u64));
    }

    #[test]
    fn normalize_wrapped_symbols() {
        assert_eq!(normalize_symbol("WETH"), "ETH");
        assert_eq!(normalize_symbol("WBTC"), "BTC");
        assert_eq!(normalize_symbol("WBNB"), "BNB");
        assert_eq!(normalize_symbol("USDC"), "USDC");
        assert_eq!(normalize_symbol("LINK"), "LINK");
        // Case-insensitive matching
        assert_eq!(normalize_symbol("weth"), "ETH");
        // Fallthrough uppercases unknown symbols
        assert_eq!(normalize_symbol("link"), "LINK");
    }

    #[test]
    fn staleness_detection() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Far in the past — stale
        assert!(check_staleness(now_ms - 60_000).is_err());

        // 1 second ago — fresh
        assert!(check_staleness(now_ms - 1_000).is_ok());
    }
}
