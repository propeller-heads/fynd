//! Individual feature computation functions for slippage-decay prediction.
//!
//! Each function is a pure computation that takes primitive or struct inputs
//! available at quote time (block X) and returns a numeric feature value.
//! Functions return `Option<f64>` when the input may be missing or invalid;
//! `None` signals "not available" and downstream consumers decide on
//! imputation strategy.
//!
//! Feature families (from the ontology):
//! - **route_topology**: hop_count, split_count, gas_estimate, pool_type_mix
//! - **temporal**: hour_of_day, day_of_week, minutes_since_hour
//! - **chain_env**: is_l2, gas_price_feature, block_utilization_feature
//! - **pool_state**: pool_liquidity_feature, fee_tier_feature, reserve_ratio
//! - **token_pair**: log_amount_ratio
//! - **route**: gas_share_of_trade

// ═══════════════════════════════════════════════════════════════════════
// Route topology features
// ═══════════════════════════════════════════════════════════════════════

/// Number of hops in the route (swap count).
///
/// Returns 0 for an empty slice (defensive — schema enforces ≥1 swap).
pub fn hop_count(swaps: &[crate::SwapRecord]) -> u32 {
    swaps.len() as u32
}

/// Number of split legs: swaps whose `split` field is strictly less than 1.0.
///
/// A route with no splits returns 0. A fully split two-leg trade returns 2.
pub fn split_count(swaps: &[crate::SwapRecord]) -> u32 {
    swaps
        .iter()
        .filter(|s| s.split < 1.0)
        .count() as u32
}

/// Parse a numeric-string gas estimate into f64.
///
/// Returns `None` for empty strings or non-numeric content.
pub fn gas_estimate_f64(gas_estimate: &str) -> Option<f64> {
    if gas_estimate.is_empty() {
        return None;
    }
    gas_estimate
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// Protocol diversity: ratio of unique protocol names to total swaps.
///
/// Returns 1.0 when every hop uses a different protocol, and approaches
/// 0.0 as swaps converge on a single protocol. Returns `None` for empty
/// input.
pub fn pool_type_diversity(swaps: &[crate::SwapRecord]) -> Option<f64> {
    if swaps.is_empty() {
        return None;
    }
    let unique: std::collections::HashSet<&str> = swaps
        .iter()
        .map(|s| s.protocol.as_str())
        .collect();
    Some(unique.len() as f64 / swaps.len() as f64)
}

// ═══════════════════════════════════════════════════════════════════════
// Temporal features
// ═══════════════════════════════════════════════════════════════════════

/// UTC hour-of-day (0–23) from a Unix timestamp.
///
/// Returns `None` if timestamp is 0 (sentinel for missing data).
pub fn hour_of_day(timestamp: u64) -> Option<u32> {
    if timestamp == 0 {
        return None;
    }
    let secs_in_day = timestamp % 86_400;
    Some((secs_in_day / 3_600) as u32)
}

/// UTC day-of-week (0 = Thursday for Unix epoch; we use ISO: 1=Mon..7=Sun).
///
/// Algorithm: Unix epoch (1970-01-01) was a Thursday (ISO day 4).
/// Returns `None` if timestamp is 0.
pub fn day_of_week(timestamp: u64) -> Option<u32> {
    if timestamp == 0 {
        return None;
    }
    // Days since epoch
    let days = timestamp / 86_400;
    // 1970-01-01 = Thursday = ISO 4
    // (days + 3) % 7 gives 0=Mon..6=Sun, then +1 for ISO 1=Mon..7=Sun
    let iso_day = ((days + 3) % 7) as u32 + 1;
    Some(iso_day)
}

/// Minutes past the current UTC hour (0–59).
///
/// Returns `None` if timestamp is 0.
pub fn minutes_since_hour(timestamp: u64) -> Option<u32> {
    if timestamp == 0 {
        return None;
    }
    let secs_in_hour = timestamp % 3_600;
    Some((secs_in_hour / 60) as u32)
}

// ═══════════════════════════════════════════════════════════════════════
// Chain environment features
// ═══════════════════════════════════════════════════════════════════════

/// Whether the chain is a Layer-2 rollup.
///
/// v1 supports Ethereum (chain_id=1, L1) and Base (chain_id=8453, L2).
pub fn is_l2(chain_id: u64) -> bool {
    // Base, Arbitrum, Optimism are L2s; Ethereum mainnet is L1
    matches!(chain_id, 8453 | 42161 | 10)
}

/// Log-scaled gas price feature from base fee in Gwei.
///
/// Uses `ln(1 + base_fee)` to compress the heavy-tailed gas price
/// distribution. Returns `None` when the base fee is unavailable or
/// negative.
pub fn gas_price_feature(base_fee_gwei: Option<f64>) -> Option<f64> {
    let fee = base_fee_gwei?;
    if fee < 0.0 || !fee.is_finite() {
        return None;
    }
    Some((1.0 + fee).ln())
}

/// Block utilization ratio: `gas_used / gas_limit`.
///
/// Returns `None` when gas_limit is zero (prevents division by zero) or
/// when either input is missing.
pub fn block_utilization_feature(gas_used: Option<u64>, gas_limit: Option<u64>) -> Option<f64> {
    let used = gas_used?;
    let limit = gas_limit?;
    if limit == 0 {
        return None;
    }
    Some(used as f64 / limit as f64)
}

// ═══════════════════════════════════════════════════════════════════════
// Pool state features
// ═══════════════════════════════════════════════════════════════════════

/// Log-scaled pool TVL in USD.
///
/// Uses `ln(1 + tvl)` to compress the heavy-tailed TVL distribution.
/// Returns `None` for missing or negative TVL values.
pub fn pool_liquidity_feature(tvl_usd: Option<f64>) -> Option<f64> {
    let tvl = tvl_usd?;
    if tvl < 0.0 || !tvl.is_finite() {
        return None;
    }
    Some((1.0 + tvl).ln())
}

/// Normalize a fee tier (given in basis points) to a [0, 1] fraction.
///
/// E.g. 30 bps → 0.003. Returns `None` when the fee tier is missing.
pub fn fee_tier_feature(fee_bps: Option<u32>) -> Option<f64> {
    let bps = fee_bps?;
    Some(f64::from(bps) / 10_000.0)
}

/// Pool reserve imbalance ratio: `|reserve_a - reserve_b| / (reserve_a + reserve_b)`.
///
/// Returns 0.0 for perfectly balanced pools, approaches 1.0 for maximally
/// imbalanced. Returns `None` when either reserve is missing or both are
/// zero.
pub fn reserve_imbalance_ratio(reserve_a: Option<f64>, reserve_b: Option<f64>) -> Option<f64> {
    let a = reserve_a?;
    let b = reserve_b?;
    if !a.is_finite() || !b.is_finite() || a < 0.0 || b < 0.0 {
        return None;
    }
    let sum = a + b;
    if sum == 0.0 {
        return None;
    }
    Some((a - b).abs() / sum)
}

// ═══════════════════════════════════════════════════════════════════════
// Token pair / route features
// ═══════════════════════════════════════════════════════════════════════

/// Log ratio of amount_out to amount_in (both as numeric strings).
///
/// Uses `ln(amount_out / amount_in)`. Useful as a continuous measure of
/// the exchange rate. Returns `None` for non-parseable or zero values.
pub fn log_amount_ratio(amount_in: &str, amount_out: &str) -> Option<f64> {
    let a_in: f64 = amount_in.parse().ok()?;
    let a_out: f64 = amount_out.parse().ok()?;
    if a_in <= 0.0 || a_out <= 0.0 || !a_in.is_finite() || !a_out.is_finite() {
        return None;
    }
    Some((a_out / a_in).ln())
}

/// Gas cost as a share of trade input size (both as numeric strings).
///
/// Returns `gas_estimate / amount_in`. Returns `None` for non-parseable,
/// zero, or negative values.
pub fn gas_share_of_trade(gas_estimate: &str, amount_in: &str) -> Option<f64> {
    let gas: f64 = gas_estimate.parse().ok()?;
    let input: f64 = amount_in.parse().ok()?;
    if input <= 0.0 || gas < 0.0 || !gas.is_finite() || !input.is_finite() {
        return None;
    }
    Some(gas / input)
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SwapRecord;

    /// Helper: build a swap record with the given protocol and split.
    fn swap(protocol: &str, split: f64) -> SwapRecord {
        SwapRecord {
            component_id: "0xdead".to_owned(),
            protocol: protocol.to_owned(),
            token_in: "0xaaa".to_owned(),
            token_out: "0xbbb".to_owned(),
            amount_in: "1000".to_owned(),
            amount_out: "900".to_owned(),
            gas_estimate: "150000".to_owned(),
            split,
        }
    }

    // ── route_topology ──────────────────────────────────────────────

    #[test]
    fn hop_count_single_swap() {
        let swaps = vec![swap("uniswap_v2", 1.0)];
        assert_eq!(hop_count(&swaps), 1);
    }

    #[test]
    fn hop_count_multi_hop() {
        let swaps = vec![swap("uniswap_v2", 1.0), swap("uniswap_v3", 1.0), swap("curve", 1.0)];
        assert_eq!(hop_count(&swaps), 3);
    }

    #[test]
    fn hop_count_empty_returns_zero() {
        assert_eq!(hop_count(&[]), 0);
    }

    #[test]
    fn split_count_no_splits() {
        let swaps = vec![swap("uniswap_v2", 1.0)];
        assert_eq!(split_count(&swaps), 0);
    }

    #[test]
    fn split_count_with_splits() {
        let swaps = vec![swap("uniswap_v2", 0.6), swap("uniswap_v3", 0.4)];
        assert_eq!(split_count(&swaps), 2);
    }

    #[test]
    fn split_count_mixed() {
        let swaps = vec![swap("uniswap_v2", 0.5), swap("uniswap_v3", 1.0)];
        assert_eq!(split_count(&swaps), 1);
    }

    #[test]
    fn split_count_empty_returns_zero() {
        assert_eq!(split_count(&[]), 0);
    }

    #[test]
    fn gas_estimate_f64_valid() {
        assert_eq!(gas_estimate_f64("150000"), Some(150_000.0));
    }

    #[test]
    fn gas_estimate_f64_zero() {
        assert_eq!(gas_estimate_f64("0"), Some(0.0));
    }

    #[test]
    fn gas_estimate_f64_empty_returns_none() {
        assert_eq!(gas_estimate_f64(""), None);
    }

    #[test]
    fn gas_estimate_f64_non_numeric_returns_none() {
        assert_eq!(gas_estimate_f64("abc"), None);
    }

    #[test]
    fn gas_estimate_f64_infinity_returns_none() {
        assert_eq!(gas_estimate_f64("inf"), None);
    }

    #[test]
    fn pool_type_diversity_single_protocol() {
        let swaps = vec![swap("uniswap_v2", 1.0), swap("uniswap_v2", 1.0)];
        assert_eq!(pool_type_diversity(&swaps), Some(0.5));
    }

    #[test]
    fn pool_type_diversity_all_different() {
        let swaps = vec![swap("uniswap_v2", 1.0), swap("uniswap_v3", 1.0), swap("curve", 1.0)];
        assert_eq!(pool_type_diversity(&swaps), Some(1.0));
    }

    #[test]
    fn pool_type_diversity_single_swap() {
        let swaps = vec![swap("balancer", 1.0)];
        assert_eq!(pool_type_diversity(&swaps), Some(1.0));
    }

    #[test]
    fn pool_type_diversity_empty_returns_none() {
        assert_eq!(pool_type_diversity(&[]), None);
    }

    // ── temporal ────────────────────────────────────────────────────

    #[test]
    fn hour_of_day_known_timestamp() {
        // 2024-10-27 14:30:00 UTC = 1730039400
        // 14:30 → hour 14
        assert_eq!(hour_of_day(1_730_039_400), Some(14));
    }

    #[test]
    fn hour_of_day_midnight() {
        // Midnight = timestamp divisible by 86400
        assert_eq!(hour_of_day(86_400), Some(0));
    }

    #[test]
    fn hour_of_day_zero_returns_none() {
        assert_eq!(hour_of_day(0), None);
    }

    #[test]
    fn day_of_week_known_date() {
        // 2024-10-28 00:00:00 UTC = Monday
        // 2024-10-28 = 1730073600
        assert_eq!(day_of_week(1_730_073_600), Some(1)); // Monday = 1
    }

    #[test]
    fn day_of_week_epoch_is_thursday() {
        // 1970-01-01 is Thursday = ISO 4
        assert_eq!(day_of_week(1), Some(4));
    }

    #[test]
    fn day_of_week_sunday() {
        // 2024-10-27 = Sunday
        // 2024-10-27 00:00:00 UTC = 1730001600
        // But let me verify: epoch + 3 days = Sunday (1970-01-04)
        // 1970-01-04 = 3 * 86400 = 259200
        assert_eq!(day_of_week(259_200), Some(7)); // Sunday = 7
    }

    #[test]
    fn day_of_week_zero_returns_none() {
        assert_eq!(day_of_week(0), None);
    }

    #[test]
    fn minutes_since_hour_known() {
        // 14:30:45 → minutes_since_hour = 30
        // secs_in_hour = 30*60 + 45 = 1845
        // 1845 / 60 = 30
        let base = 86_400 + 14 * 3_600 + 30 * 60 + 45;
        assert_eq!(minutes_since_hour(base), Some(30));
    }

    #[test]
    fn minutes_since_hour_exact_hour() {
        let ts = 86_400 + 14 * 3_600; // exactly 14:00:00
        assert_eq!(minutes_since_hour(ts), Some(0));
    }

    #[test]
    fn minutes_since_hour_zero_returns_none() {
        assert_eq!(minutes_since_hour(0), None);
    }

    // ── chain_env ──────────────────────────────────────────────────

    #[test]
    fn is_l2_ethereum_mainnet() {
        assert!(!is_l2(1));
    }

    #[test]
    fn is_l2_base_chain() {
        assert!(is_l2(8453));
    }

    #[test]
    fn is_l2_arbitrum() {
        assert!(is_l2(42161));
    }

    #[test]
    fn is_l2_unknown_chain() {
        assert!(!is_l2(999));
    }

    #[test]
    fn gas_price_feature_normal() {
        // base_fee = 30 Gwei → ln(31) ≈ 3.434
        let result = gas_price_feature(Some(30.0));
        assert!(result.is_some());
        let val = result.unwrap();
        assert!((val - (31.0_f64).ln()).abs() < 1e-10);
    }

    #[test]
    fn gas_price_feature_zero_base_fee() {
        // base_fee = 0 → ln(1) = 0
        assert_eq!(gas_price_feature(Some(0.0)), Some(0.0));
    }

    #[test]
    fn gas_price_feature_none_returns_none() {
        assert_eq!(gas_price_feature(None), None);
    }

    #[test]
    fn gas_price_feature_negative_returns_none() {
        assert_eq!(gas_price_feature(Some(-5.0)), None);
    }

    #[test]
    fn gas_price_feature_infinity_returns_none() {
        assert_eq!(gas_price_feature(Some(f64::INFINITY)), None);
    }

    #[test]
    fn gas_price_feature_nan_returns_none() {
        assert_eq!(gas_price_feature(Some(f64::NAN)), None);
    }

    #[test]
    fn block_utilization_normal() {
        // 15M used out of 30M limit → 0.5
        let result = block_utilization_feature(Some(15_000_000), Some(30_000_000));
        assert_eq!(result, Some(0.5));
    }

    #[test]
    fn block_utilization_full_block() {
        let result = block_utilization_feature(Some(30_000_000), Some(30_000_000));
        assert_eq!(result, Some(1.0));
    }

    #[test]
    fn block_utilization_empty_block() {
        let result = block_utilization_feature(Some(0), Some(30_000_000));
        assert_eq!(result, Some(0.0));
    }

    #[test]
    fn block_utilization_zero_limit_returns_none() {
        assert_eq!(block_utilization_feature(Some(100), Some(0)), None);
    }

    #[test]
    fn block_utilization_missing_used_returns_none() {
        assert_eq!(block_utilization_feature(None, Some(30_000_000)), None);
    }

    #[test]
    fn block_utilization_missing_limit_returns_none() {
        assert_eq!(block_utilization_feature(Some(15_000_000), None), None);
    }

    #[test]
    fn block_utilization_both_missing_returns_none() {
        assert_eq!(block_utilization_feature(None, None), None);
    }

    // ── pool_state ─────────────────────────────────────────────────

    #[test]
    fn pool_liquidity_feature_normal() {
        // tvl = 1M → ln(1_000_001) ≈ 13.815
        let result = pool_liquidity_feature(Some(1_000_000.0));
        assert!(result.is_some());
        let val = result.unwrap();
        assert!((val - (1_000_001.0_f64).ln()).abs() < 1e-6);
    }

    #[test]
    fn pool_liquidity_feature_zero_tvl() {
        // tvl = 0 → ln(1) = 0
        assert_eq!(pool_liquidity_feature(Some(0.0)), Some(0.0));
    }

    #[test]
    fn pool_liquidity_feature_none_returns_none() {
        assert_eq!(pool_liquidity_feature(None), None);
    }

    #[test]
    fn pool_liquidity_feature_negative_returns_none() {
        assert_eq!(pool_liquidity_feature(Some(-100.0)), None);
    }

    #[test]
    fn pool_liquidity_feature_infinity_returns_none() {
        assert_eq!(pool_liquidity_feature(Some(f64::INFINITY)), None);
    }

    #[test]
    fn fee_tier_feature_30bps() {
        // 30 bps = 0.003
        assert_eq!(fee_tier_feature(Some(30)), Some(0.003));
    }

    #[test]
    fn fee_tier_feature_100bps() {
        // 100 bps = 0.01
        assert_eq!(fee_tier_feature(Some(100)), Some(0.01));
    }

    #[test]
    fn fee_tier_feature_zero_bps() {
        assert_eq!(fee_tier_feature(Some(0)), Some(0.0));
    }

    #[test]
    fn fee_tier_feature_none_returns_none() {
        assert_eq!(fee_tier_feature(None), None);
    }

    #[test]
    fn reserve_imbalance_balanced() {
        // Equal reserves → 0.0
        let result = reserve_imbalance_ratio(Some(100.0), Some(100.0));
        assert_eq!(result, Some(0.0));
    }

    #[test]
    fn reserve_imbalance_skewed() {
        // 90 vs 10 → |80| / 100 = 0.8
        let result = reserve_imbalance_ratio(Some(90.0), Some(10.0));
        assert_eq!(result, Some(0.8));
    }

    #[test]
    fn reserve_imbalance_order_independent() {
        let a = reserve_imbalance_ratio(Some(90.0), Some(10.0));
        let b = reserve_imbalance_ratio(Some(10.0), Some(90.0));
        assert_eq!(a, b);
    }

    #[test]
    fn reserve_imbalance_both_zero_returns_none() {
        assert_eq!(reserve_imbalance_ratio(Some(0.0), Some(0.0)), None);
    }

    #[test]
    fn reserve_imbalance_missing_a_returns_none() {
        assert_eq!(reserve_imbalance_ratio(None, Some(100.0)), None);
    }

    #[test]
    fn reserve_imbalance_missing_b_returns_none() {
        assert_eq!(reserve_imbalance_ratio(Some(100.0), None), None);
    }

    #[test]
    fn reserve_imbalance_negative_returns_none() {
        assert_eq!(reserve_imbalance_ratio(Some(-1.0), Some(100.0)), None);
    }

    #[test]
    fn reserve_imbalance_infinity_returns_none() {
        assert_eq!(reserve_imbalance_ratio(Some(f64::INFINITY), Some(100.0)), None);
    }

    // ── token_pair / route ─────────────────────────────────────────

    #[test]
    fn log_amount_ratio_equal() {
        // 1000 in, 1000 out → ln(1) = 0
        assert_eq!(log_amount_ratio("1000", "1000"), Some(0.0));
    }

    #[test]
    fn log_amount_ratio_typical() {
        // 1e18 in, 3.5e9 out → ln(3.5e9 / 1e18) = ln(3.5e-9)
        let result = log_amount_ratio("1000000000000000000", "3500000000");
        assert!(result.is_some());
        let expected = (3_500_000_000.0_f64 / 1e18).ln();
        assert!((result.unwrap() - expected).abs() < 1e-6);
    }

    #[test]
    fn log_amount_ratio_zero_in_returns_none() {
        assert_eq!(log_amount_ratio("0", "1000"), None);
    }

    #[test]
    fn log_amount_ratio_zero_out_returns_none() {
        assert_eq!(log_amount_ratio("1000", "0"), None);
    }

    #[test]
    fn log_amount_ratio_non_numeric_returns_none() {
        assert_eq!(log_amount_ratio("abc", "1000"), None);
    }

    #[test]
    fn log_amount_ratio_empty_string_returns_none() {
        assert_eq!(log_amount_ratio("", "1000"), None);
    }

    #[test]
    fn gas_share_of_trade_normal() {
        // gas=150000, amount_in=1e18 → 150000/1e18 = 1.5e-13
        let result = gas_share_of_trade("150000", "1000000000000000000");
        assert!(result.is_some());
        let expected = 150_000.0 / 1e18;
        assert!((result.unwrap() - expected).abs() < 1e-20);
    }

    #[test]
    fn gas_share_of_trade_zero_gas() {
        // gas=0 is valid → returns 0.0
        assert_eq!(gas_share_of_trade("0", "1000"), Some(0.0));
    }

    #[test]
    fn gas_share_of_trade_zero_amount_returns_none() {
        assert_eq!(gas_share_of_trade("150000", "0"), None);
    }

    #[test]
    fn gas_share_of_trade_non_numeric_returns_none() {
        assert_eq!(gas_share_of_trade("xyz", "1000"), None);
    }

    #[test]
    fn gas_share_of_trade_empty_returns_none() {
        assert_eq!(gas_share_of_trade("", "1000"), None);
    }

    #[test]
    fn gas_share_of_trade_negative_gas_returns_none() {
        assert_eq!(gas_share_of_trade("-100", "1000"), None);
    }
}
