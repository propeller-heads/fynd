//! Basis-point arithmetic, shared by the modules that scale an amount by a rate.

use num_bigint::BigUint;

/// Basis points in one whole — the denominator every rate here divides by.
pub(crate) const DENOMINATOR: u32 = 10_000;

/// Scales `amount` by `rate_bps / 10_000`, truncating.
///
/// Truncation moves the result toward zero, so a caller using this for a lower bound gets a looser
/// bound and one using it for an upper bound gets a tighter one.
pub(crate) fn scale_truncating(amount: &BigUint, rate_bps: u32) -> BigUint {
    (amount * BigUint::from(rate_bps)) / BigUint::from(DENOMINATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_truncating_rounds_toward_zero() {
        // 99 · 101% = 99.99, which truncates to 99 rather than rounding to 100.
        assert_eq!(
            scale_truncating(&BigUint::from(99u32), DENOMINATOR + 100),
            BigUint::from(99u32)
        );
        assert_eq!(
            scale_truncating(&BigUint::from(1_000_000u32), DENOMINATOR - 300),
            BigUint::from(970_000u32)
        );
    }

    #[test]
    fn test_scale_truncating_zero_amount() {
        assert_eq!(scale_truncating(&BigUint::ZERO, DENOMINATOR + 100), BigUint::ZERO);
    }
}
