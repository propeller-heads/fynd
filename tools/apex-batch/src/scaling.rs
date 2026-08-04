//! Decimal scaling between a token's native precision and APEX's 18-decimal contract.
//!
//! APEX's core bakes 18 decimals in: `truncate_to_precision`, `remove_extra_precision` and
//! `validate_result` all key off `Token.decimals`, and every amount the solver reasons about is a
//! token amount already scaled to 18. Raising the core to native decimals is a separate roadmap
//! track — this module is the hardened boundary we control in the meantime.
//!
//! "Hardened" is the whole point of the module, and it is one rule: **an amount we cannot scale
//! is declined, never approximated and never fatal.** Turbine's equivalent asserts on
//! `decimals <= 18` and unwraps the multiply, so a single 24-decimal token or one oversized
//! amount aborts the batch. Here both are [`ScaleError`]s that the runner counts as an exclusion
//! reason, so a study run reports "N orders excluded, M of them for scaling" instead of dying —
//! the coverage number is part of the result, not an obstacle to producing one.
//!
//! Rounding follows the direction that cannot manufacture value: amounts the trader *sends*
//! round up ([`TokenScale::scale_down_ceil`]), amounts the trader *receives* round down
//! ([`TokenScale::scale_down_floor`]). Getting these backwards leaks a sub-unit of value per
//! order in APEX's favour, which is exactly the direction this study must not err in.

use alloy::primitives::U256;

/// APEX's fixed working precision. Every amount inside a batch is a [`Scaled18`].
pub const APEX_DECIMALS: u8 = 18;

/// A token amount expressed in APEX's 18-decimal working precision.
///
/// The newtype exists because 18-decimal and native-decimal amounts are both `U256` and mixing
/// them is silent: a `U256` handed to APEX unscaled is off by a factor of 10^12 for USDC and the
/// batch still clears, just wrongly. Every conversion in this module goes through the type, so a
/// native amount cannot reach APEX without passing a [`TokenScale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scaled18(pub U256);

/// Why an amount could not be moved between native and 18-decimal precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScaleError {
    /// The token declares more than 18 decimals, so scaling up would have to *divide* and lose
    /// low-order digits. APEX cannot represent the token faithfully; the order is excluded.
    #[error("token has {decimals} decimals; APEX's working precision is {APEX_DECIMALS}")]
    DecimalsAbove18 { decimals: u8 },
    /// Scaling up overflowed `U256`. Reachable for a low-decimal token with an absurd notional
    /// (the 10^12 factor on a 6-decimal token eats 40 bits), and for garbage amounts from a
    /// mis-decoded trade.
    #[error("scaling {amount} up by 10^{shift} overflows U256")]
    Overflow { amount: U256, shift: u8 },
}

/// The scaling rule for one token, derived from its decimals.
///
/// Constructed once per token per block rather than passed as a bare `u8`, so the
/// more-than-18-decimals check happens at one place — the boundary — instead of at every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenScale {
    decimals: u8,
}

impl TokenScale {
    /// The scaling rule for a token with `decimals` native decimals.
    ///
    /// Declines above 18 immediately: a `TokenScale` that exists is one every amount can be
    /// scaled through, so the callers below only have to handle overflow.
    pub fn new(decimals: u8) -> Result<Self, ScaleError> {
        if decimals > APEX_DECIMALS {
            return Err(ScaleError::DecimalsAbove18 { decimals });
        }
        Ok(Self { decimals })
    }

    /// The token's native decimals.
    pub fn decimals(self) -> u8 {
        self.decimals
    }

    /// Powers of ten this scale multiplies by, i.e. `18 - decimals`.
    fn shift(self) -> u8 {
        APEX_DECIMALS - self.decimals
    }

    /// The multiplier `10^(18 - decimals)`. Always representable: the largest shift is 18.
    fn factor(self) -> U256 {
        U256::from(10u64).pow(U256::from(self.shift()))
    }

    /// Lift a native-precision amount into APEX's 18-decimal space.
    ///
    /// Checked, not wrapping: an overflowing amount is a declined order, not a wrapped one that
    /// would clear at a nonsense price.
    pub fn scale_up(self, amount: U256) -> Result<Scaled18, ScaleError> {
        amount
            .checked_mul(self.factor())
            .map(Scaled18)
            .ok_or(ScaleError::Overflow { amount, shift: self.shift() })
    }

    /// Lower an 18-decimal amount to native precision, rounding **down**.
    ///
    /// For amounts the trader receives (buy amounts): rounding down can only under-credit the
    /// batch, never promise an output the pools cannot deliver.
    pub fn scale_down_floor(self, amount: Scaled18) -> U256 {
        amount.0 / self.factor()
    }

    /// Lower an 18-decimal amount to native precision, rounding **up**.
    ///
    /// For amounts the trader sends (sell amounts): rounding up can only over-charge the batch,
    /// never let it take more input than the order authorised.
    pub fn scale_down_ceil(self, amount: Scaled18) -> U256 {
        let factor = self.factor();
        let quotient = amount.0 / factor;
        if (amount.0 % factor).is_zero() {
            quotient
        } else {
            quotient + U256::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `value * 10^decimals`, for writing amounts the way a human reads them.
    fn units(value: u64, decimals: u8) -> U256 {
        U256::from(value) * U256::from(10u64).pow(U256::from(decimals))
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_scale_up_round_trips_through_floor() {
        // Round-trip identity: a native amount scaled up and floored back is itself, for every
        // decimal count APEX can represent. The batch must not lose a unit just by entering it.
        for decimals in 0..=APEX_DECIMALS {
            let scale = TokenScale::new(decimals).expect("decimals within APEX precision");
            let native = units(1_234, decimals);
            let scaled = scale
                .scale_up(native)
                .expect("1234 tokens is far from overflow");
            assert_eq!(scale.scale_down_floor(scaled), native, "decimals={decimals}");
            assert_eq!(scale.scale_down_ceil(scaled), native, "decimals={decimals}");
        }
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_scale_down_floor_never_exceeds_ceil() {
        let usdc = TokenScale::new(6).expect("6 decimals is within APEX precision");
        // One sub-unit of dust below a whole USDC unit: the two roundings must straddle it, or
        // the sell/buy asymmetry that keeps the batch honest does not exist.
        let dusty = Scaled18(units(1, 18) + U256::from(1));
        let floor = usdc.scale_down_floor(dusty);
        let ceil = usdc.scale_down_ceil(dusty);
        assert!(floor <= ceil, "floor {floor} must not exceed ceil {ceil}");
        assert_eq!(floor, units(1, 6));
        assert_eq!(ceil, units(1, 6) + U256::from(1));

        // Exact multiples must agree: ceil only rounds up on a real remainder.
        let exact = Scaled18(units(1, 18));
        assert_eq!(usdc.scale_down_floor(exact), usdc.scale_down_ceil(exact));
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_decimals_above_18_are_declined() {
        // Turbine's equivalent asserts here, which aborts the whole batch. A 24-decimal token
        // must cost its own order and nothing else.
        assert_eq!(TokenScale::new(24), Err(ScaleError::DecimalsAbove18 { decimals: 24 }));
        assert_eq!(
            TokenScale::new(APEX_DECIMALS + 1),
            Err(ScaleError::DecimalsAbove18 { decimals: APEX_DECIMALS + 1 })
        );
        assert!(TokenScale::new(APEX_DECIMALS).is_ok(), "18 decimals needs no scaling at all");
        assert!(TokenScale::new(0).is_ok(), "a 0-decimal token is scaled, not rejected");
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_overflowing_scale_up_is_declined() {
        let usdc = TokenScale::new(6).expect("6 decimals is within APEX precision");
        // U256::MAX cannot survive a 10^12 multiply. The result must be an error, not a wrapped
        // value that would clear the batch at a fabricated price.
        assert_eq!(
            usdc.scale_up(U256::MAX),
            Err(ScaleError::Overflow { amount: U256::MAX, shift: 12 })
        );
        // The largest amount that still fits scales cleanly, so the check is a boundary and not
        // a blanket refusal of large trades.
        let largest = U256::MAX / U256::from(10u64).pow(U256::from(12u8));
        assert!(usdc.scale_up(largest).is_ok());
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_18_decimal_scaling_is_identity() {
        let weth = TokenScale::new(18).expect("18 decimals is APEX's own precision");
        let amount = units(7, 18) + U256::from(3);
        let scaled = weth
            .scale_up(amount)
            .expect("no multiply happens at 18 decimals");
        assert_eq!(scaled, Scaled18(amount));
        assert_eq!(weth.scale_down_floor(scaled), amount);
        assert_eq!(weth.scale_down_ceil(scaled), amount);
    }
}
