//! Cost math: compute the signed error between Fynd's gas estimate and the
//! simulator's measurement, multiplied by a constant gas price.

use num_bigint::BigUint;
use num_traits::ToPrimitive;

pub struct CostBreakdown {
    pub error_gas: i128,
    pub error_wei: i128,
    pub error_eth: f64,
}

/// Compute the gas-units delta and its ETH cost at the given gas price.
///
/// Positive error = Fynd over-charged (estimate > actual).
/// Negative error = Fynd under-charged (estimate < actual).
pub fn compute(
    gas_estimate: &BigUint,
    actual_gas: u64,
    gas_price_wei: &BigUint,
) -> anyhow::Result<CostBreakdown> {
    let est_i128: i128 = gas_estimate
        .to_i128()
        .ok_or_else(|| anyhow::anyhow!("gas_estimate overflows i128: {gas_estimate}"))?;
    let actual_i128: i128 = i128::from(actual_gas);
    let error_gas = est_i128 - actual_i128;

    let price_i128: i128 = gas_price_wei
        .to_i128()
        .ok_or_else(|| anyhow::anyhow!("gas_price overflows i128: {gas_price_wei}"))?;
    let error_wei = error_gas
        .checked_mul(price_i128)
        .ok_or_else(|| anyhow::anyhow!("error_wei overflow: {error_gas} * {price_i128}"))?;

    #[allow(clippy::cast_precision_loss)]
    let error_eth = (error_wei as f64) / 1e18;
    Ok(CostBreakdown { error_gas, error_wei, error_eth })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_estimate_produces_positive_error() {
        let result = compute(
            &BigUint::from(200_000u64),
            150_000,
            &BigUint::from(20_000_000_000u64), // 20 gwei
        )
        .unwrap();
        assert_eq!(result.error_gas, 50_000);
        assert_eq!(result.error_wei, 50_000 * 20_000_000_000);
        assert!((result.error_eth - 0.001).abs() < 1e-15);
    }

    #[test]
    fn under_estimate_produces_negative_error() {
        let result = compute(
            &BigUint::from(100_000u64),
            150_000,
            &BigUint::from(10_000_000_000u64), // 10 gwei
        )
        .unwrap();
        assert_eq!(result.error_gas, -50_000);
        assert_eq!(result.error_wei, -50_000 * 10_000_000_000);
        assert!(result.error_eth < 0.0);
    }

    #[test]
    fn exact_match_zero_error() {
        let result =
            compute(&BigUint::from(123_456u64), 123_456, &BigUint::from(30_000_000_000u64))
                .unwrap();
        assert_eq!(result.error_gas, 0);
        assert_eq!(result.error_wei, 0);
        assert_eq!(result.error_eth, 0.0);
    }
}
