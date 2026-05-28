//! Select the top-N token pairs and representative amounts from a trade dataset.
//!
//! Used by the `audit` subcommand to derive what to benchmark from the existing
//! aggregator trade dataset rather than requiring separate configuration.

use std::collections::HashMap;

use num_bigint::BigUint;

use crate::requests::{lookup_symbol, SwapRequest};

/// A token pair with pre-computed representative trade amounts.
#[derive(Debug, Clone)]
pub struct PairSpec {
    /// Human-readable label, e.g. `"WETH → USDC"`.
    pub label: String,
    /// Checksummed (or lower-case) ERC-20 address of the sell token.
    pub token_in: String,
    /// Checksummed (or lower-case) ERC-20 address of the buy token.
    pub token_out: String,
    /// `amounts_per_pair` representative amounts derived from percentiles of real trades.
    pub amounts: Vec<String>,
}

/// Returns `(decimals, approx_usd_price_per_whole_token)` for well-known tokens.
///
/// Prices are intentionally rough — within an order of magnitude is sufficient to give a
/// sensible USD floor. Returns `None` for unknown tokens, which bypasses the filter.
fn token_usd_price(addr: &str) -> Option<(u32, f64)> {
    match addr {
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" => Some((18, 2_500.0)), // WETH
        "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee" => Some((18, 2_500.0)), // ETH
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => Some((6, 1.0)),      // USDC
        "0xdac17f958d2ee523a2206206994597c13d831ec7" => Some((6, 1.0)),      // USDT
        "0x6b175474e89094c44da98b954eedeac495271d0f" => Some((18, 1.0)),     // DAI
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => Some((8, 95_000.0)), // WBTC
        "0x7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9" => Some((18, 200.0)),   // AAVE
        "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984" => Some((18, 10.0)),    // UNI
        "0x514910771af9ca656af840dff83e8264ecf986ca" => Some((18, 15.0)),    // LINK
        "0x6982508145454ce325ddbe47a25d4ec3d2311933" => Some((18, 0.000015)), // PEPE
        "0x95ad61b0a150d79219dcf64e1e6cc01f0b64c4ce" => Some((18, 0.00002)), // SHIB
        "0x6810e776880c02933d47db1b9fc05908e5386b96" => Some((18, 150.0)),   // GNO
        _ => None,
    }
}

/// Returns whether `raw_amount` of `token_addr` meets the minimum USD threshold.
///
/// Always returns `true` for unknown tokens (no price data available, keep without filtering).
fn meets_min_usd(token_addr: &str, raw_amount: &BigUint, min_usd: f64) -> bool {
    if min_usd <= 0.0 {
        return true;
    }
    let Some((decimals, usd_price)) = token_usd_price(token_addr) else {
        return true;
    };
    let amount_f64: f64 = raw_amount
        .to_string()
        .parse()
        .unwrap_or(f64::MAX);
    let usd_value = amount_f64 / 10f64.powi(decimals as i32) * usd_price;
    usd_value >= min_usd
}

/// Derive the top-`top_n` pairs by trade frequency and compute `amounts_per_pair` percentile
/// amounts per pair from the supplied trade dataset.
///
/// Pairs are kept direction-specific: WETH→USDC and USDC→WETH are counted separately.
/// When the dataset has fewer than `top_n` unique pairs, all pairs are returned.
///
/// `min_amount_usd` drops individual trades below the approximate USD threshold from the
/// amount distribution before computing percentiles. Set to `0.0` to disable.
pub fn select_top_pairs(
    trades: &[SwapRequest],
    top_n: usize,
    amounts_per_pair: usize,
    min_amount_usd: f64,
) -> Vec<PairSpec> {
    // Accumulate all trade amounts per (token_in, token_out) pair.
    let mut pair_amounts: HashMap<(String, String), Vec<BigUint>> = HashMap::new();
    for trade in trades {
        let key = (trade.token_in_addr().to_lowercase(), trade.token_out_addr().to_lowercase());
        if let Ok(amount) = trade.raw_amount().parse::<BigUint>() {
            if amount > BigUint::default() && meets_min_usd(&key.0, &amount, min_amount_usd) {
                pair_amounts
                    .entry(key)
                    .or_default()
                    .push(amount);
            }
        }
    }

    // Sort by trade count descending, then by (token_in, token_out) for determinism.
    let mut ranked: Vec<_> = pair_amounts.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then_with(|| a.0.cmp(&b.0))
    });

    ranked
        .into_iter()
        .take(top_n)
        .map(|((token_in, token_out), mut amounts)| {
            amounts.sort_unstable();
            let in_sym = lookup_symbol(&token_in);
            let out_sym = lookup_symbol(&token_out);
            PairSpec {
                label: format!("{in_sym} → {out_sym}"),
                amounts: percentile_sample(&amounts, amounts_per_pair),
                token_in,
                token_out,
            }
        })
        .collect()
}

/// Sample `n` values at evenly-spaced percentiles from a pre-sorted slice.
///
/// With `n = 10` this produces the 0th, ~11th, ~22nd … 100th percentile values,
/// giving a representative spread across the real trade-size distribution.
fn percentile_sample(sorted: &[BigUint], n: usize) -> Vec<String> {
    if sorted.is_empty() || n == 0 {
        return vec![];
    }
    let n = n.max(1);
    (0..n)
        .map(|i| {
            let frac = if n == 1 { 0.5_f64 } else { i as f64 / (n - 1) as f64 };
            let idx = ((sorted.len() - 1) as f64 * frac).round() as usize;
            sorted[idx.min(sorted.len() - 1)].to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trade(token_in: &str, token_out: &str, amount: &str) -> SwapRequest {
        use crate::requests::load_request_templates;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pair_selector_test_{}.json", fastrand::u64(..)));
        let content = serde_json::json!([{
            "orders": [{
                "token_in": token_in,
                "token_out": token_out,
                "amount": amount,
                "side": "sell",
                "sender": "0x0000000000000000000000000000000000000001"
            }]
        }]);
        std::fs::write(&path, content.to_string()).unwrap();
        let mut reqs = load_request_templates(path.to_str().unwrap(), 5000).unwrap();
        std::fs::remove_file(&path).ok();
        reqs.remove(0)
    }

    #[test]
    fn percentile_sample_single() {
        assert_eq!(percentile_sample(&[BigUint::from(100u32)], 1), vec!["100"]);
    }

    #[test]
    fn percentile_sample_endpoints() {
        let nums: Vec<BigUint> = [10u32, 20, 30, 40, 50]
            .map(BigUint::from)
            .to_vec();
        let s = percentile_sample(&nums, 2);
        assert_eq!(s, vec!["10", "50"]);
    }

    #[test]
    fn percentile_sample_n_greater_than_data() {
        let nums: Vec<BigUint> = [10u32, 20].map(BigUint::from).to_vec();
        let s = percentile_sample(&nums, 5);
        assert_eq!(s.len(), 5);
        // Min and max must always be included.
        assert_eq!(s[0], "10");
        assert_eq!(s[4], "20");
    }

    #[test]
    fn select_top_pairs_ranks_by_frequency() {
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let dai = "0x6b175474e89094c44da98b954eedeac495271d0f";

        let trades = vec![
            make_trade(weth, usdc, "1000000000000000000"),
            make_trade(weth, usdc, "2000000000000000000"),
            make_trade(weth, usdc, "3000000000000000000"),
            make_trade(weth, dai, "500000000000000000"),
        ];

        let pairs = select_top_pairs(&trades, 2, 3, 0.0);
        assert_eq!(pairs.len(), 2);
        // WETH→USDC has 3 trades and should rank first.
        assert_eq!(pairs[0].token_in, weth);
        assert_eq!(pairs[0].token_out, usdc);
        assert_eq!(pairs[0].amounts.len(), 3);
    }

    #[test]
    fn select_top_pairs_respects_top_n_cap() {
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let trades = vec![make_trade(weth, usdc, "1000000000000000000")];
        let pairs = select_top_pairs(&trades, 10, 1, 0.0);
        // Only 1 unique pair exists.
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn select_top_pairs_empty_input() {
        let pairs = select_top_pairs(&[], 10, 10, 0.0);
        assert!(pairs.is_empty());
    }

    #[test]
    fn min_amount_usd_filters_dust() {
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        // dust: 0.001 WETH ≈ $2.50, below $250 threshold
        let dust = "1000000000000000";
        // real: 0.1 WETH ≈ $250, at threshold
        let real = "100000000000000000";
        let trades = vec![make_trade(weth, usdc, dust), make_trade(weth, usdc, real)];
        // Without filter: both amounts included
        let pairs_all = select_top_pairs(&trades, 1, 2, 0.0);
        assert_eq!(pairs_all[0].amounts.len(), 2);
        // With $250 filter: only the real amount survives; percentile_sample still returns n
        // values but all are the same (single-point distribution).
        let pairs_filtered = select_top_pairs(&trades, 1, 2, 250.0);
        assert_eq!(pairs_filtered[0].amounts.len(), 2);
        assert!(pairs_filtered[0]
            .amounts
            .iter()
            .all(|a| a == real));
        // The dust amount must not appear.
        assert!(!pairs_filtered[0]
            .amounts
            .contains(&dust.to_string()));
    }

    #[test]
    fn min_amount_usd_unknown_token_passes_through() {
        let unknown = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        // tiny raw amount — but unknown token, so no price to check
        let trades = vec![make_trade(unknown, usdc, "1")];
        let pairs = select_top_pairs(&trades, 1, 1, 250.0);
        assert_eq!(pairs.len(), 1, "unknown token should not be filtered");
    }
}
