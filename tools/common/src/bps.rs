//! Basis-point comparison math between two quote amounts.

use num_bigint::BigUint;

/// Compare baseline vs another participant on raw output amounts, in basis points.
///
/// Positive = baseline delivers more output. `None` when amounts are zero or unparseable.
pub fn raw_bps_diff(baseline: &str, other: &str) -> Option<f64> {
    let b: f64 = baseline
        .parse()
        .ok()
        .filter(|&v: &f64| v > 0.0)?;
    let o: f64 = other
        .parse()
        .ok()
        .filter(|&v: &f64| v > 0.0)?;
    Some((b - o) / o * 10_000.0)
}

/// Compare baseline vs another participant net-of-gas, in basis points.
///
/// Derives the token-per-gas-unit price from the baseline's `(raw - net_gas) / gas_units`,
/// then applies it to the other participant's gas units. Returns `None` when either gas
/// figure is zero, when `baseline_net_gas` is absent, or when gas exceeds 5% of output.
/// Call this twice with reported and on-chain gas values to keep the two measurements
/// independent.
///
/// **Note:** deriving a per-gas token cost from `(raw - net_gas) / gas_units` is a hack that
/// compensates for the absence of a gas price denominated in the output token. The result is only
/// meaningful when the solver's gas estimate is accurate.
pub fn gas_adjusted_bps_diff(
    baseline_raw: &str,
    baseline_net_gas: Option<&str>,
    baseline_gas_units: u64,
    other_raw: &str,
    other_gas_units: u64,
) -> Option<f64> {
    let baseline_net_gas = baseline_net_gas?;
    if baseline_gas_units == 0 || other_gas_units == 0 {
        return None;
    }
    let b_raw: f64 = baseline_raw
        .parse()
        .ok()
        .filter(|&v: &f64| v > 0.0)?;
    let b_net: f64 = baseline_net_gas
        .parse()
        .ok()
        .filter(|&v: &f64| v > 0.0)?;
    let o_raw: f64 = other_raw
        .parse()
        .ok()
        .filter(|&v: &f64| v > 0.0)?;

    let gas_cost = b_raw - b_net;

    // Skip trades where gas exceeds 5% of output — uneconomical and the bps math becomes noise.
    if gas_cost * 20.0 > b_raw {
        return None;
    }

    let token_per_gas = gas_cost / baseline_gas_units as f64;
    let o_net = o_raw - (other_gas_units as f64 * token_per_gas);

    if o_net <= 0.0 {
        return None;
    }
    Some((b_net - o_net) / o_net * 10_000.0)
}

/// Diff between an `eth_call` on-chain result and a quoted amount, in basis points.
///
/// Positive = on-chain result exceeds the quoted amount (the quote was conservative).
/// Negative = on-chain result is less than quoted (the quote was optimistic).
/// `None` when either amount is zero.
pub fn eth_call_bps_diff(actual: &BigUint, quoted: &BigUint) -> Option<f64> {
    if *actual == BigUint::ZERO || *quoted == BigUint::ZERO {
        return None;
    }
    let a: f64 = actual
        .to_string()
        .parse()
        .unwrap_or(0.0);
    let q: f64 = quoted
        .to_string()
        .parse()
        .unwrap_or(0.0);
    if q == 0.0 {
        return None;
    }
    Some((a - q) / q * 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_bps_diff_baseline_better() {
        let d = raw_bps_diff("10100", "10000").unwrap();
        assert!((d - 100.0).abs() < 0.01, "expected 100 bps, got {d}");
    }

    #[test]
    fn raw_bps_diff_other_better() {
        let d = raw_bps_diff("9900", "10000").unwrap();
        assert!((d - (-100.0)).abs() < 0.01, "expected -100 bps, got {d}");
    }

    #[test]
    fn raw_bps_diff_equal() {
        assert_eq!(raw_bps_diff("10000", "10000"), Some(0.0));
    }

    #[test]
    fn raw_bps_diff_zero_other() {
        assert_eq!(raw_bps_diff("10000", "0"), None);
    }

    #[test]
    fn raw_bps_diff_invalid() {
        assert_eq!(raw_bps_diff("abc", "10000"), None);
    }

    #[test]
    fn gas_adjusted_requires_baseline_net_gas() {
        assert_eq!(gas_adjusted_bps_diff("10000", None, 100, "10000", 100), None);
    }

    #[test]
    fn gas_adjusted_zero_gas_units() {
        assert_eq!(gas_adjusted_bps_diff("10000", Some("9990"), 0, "10000", 100), None);
        assert_eq!(gas_adjusted_bps_diff("10000", Some("9990"), 100, "10000", 0), None);
    }

    #[test]
    fn gas_adjusted_skips_uneconomical_gas() {
        // gas_cost = 10000 - 9000 = 1000, which is 10% of output (> 5% threshold) → skipped.
        assert_eq!(gas_adjusted_bps_diff("10000", Some("9000"), 100, "10000", 100), None);
    }

    #[test]
    fn gas_adjusted_equal_gas_matches_raw() {
        // Same raw and same gas units → net-of-gas diff equals the raw diff (0 bps).
        let d = gas_adjusted_bps_diff("10000", Some("9990"), 100, "10000", 100).unwrap();
        assert!(d.abs() < 0.01, "expected ~0 bps, got {d}");
    }

    #[test]
    fn gas_adjusted_other_uses_more_gas() {
        // Baseline pays 10 tokens over 100 gas (0.1 token/gas). The other uses 200 gas → 20
        // tokens of gas → net 9980 vs baseline net 9990 → baseline better by a positive bps.
        let d = gas_adjusted_bps_diff("10000", Some("9990"), 100, "10000", 200).unwrap();
        assert!(d > 0.0, "baseline should be better net-of-gas, got {d}");
    }

    #[test]
    fn eth_call_bps_diff_positive_when_actual_exceeds_quote() {
        let actual: BigUint = "10100".parse().unwrap();
        let quoted: BigUint = "10000".parse().unwrap();
        let d = eth_call_bps_diff(&actual, &quoted).unwrap();
        assert!((d - 100.0).abs() < 0.01, "expected 100 bps, got {d}");
    }

    #[test]
    fn eth_call_bps_diff_negative_when_actual_below_quote() {
        let actual: BigUint = "9900".parse().unwrap();
        let quoted: BigUint = "10000".parse().unwrap();
        let d = eth_call_bps_diff(&actual, &quoted).unwrap();
        assert!((d - (-100.0)).abs() < 0.01, "expected -100 bps, got {d}");
    }

    #[test]
    fn eth_call_bps_diff_zero_amount_is_none() {
        let zero = BigUint::ZERO;
        let ten_k: BigUint = "10000".parse().unwrap();
        assert_eq!(eth_call_bps_diff(&zero, &ten_k), None);
        assert_eq!(eth_call_bps_diff(&ten_k, &zero), None);
    }
}
