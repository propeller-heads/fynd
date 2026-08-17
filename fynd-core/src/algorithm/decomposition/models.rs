use std::sync::Arc;

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::Price};

use crate::{
    algorithm::decomposition::components::SPLIT_PRECISION, derived::TokenGasPrices, ComponentId,
};

/// Gas cost expressed in a token.
///
/// defibot passes `dict[symbol, Decimal]` holding, per token, the price of one gas unit denominated
/// in that token (`optimizers/interface.py:13`). Fynd splits the same quantity in two: the block's
/// gas price in wei, and [`TokenGasPrices`] mapping a token to its wei ratio.
#[derive(Clone)]
pub(crate) struct TokenPriceData {
    pub(crate) gas_price_wei: BigUint,
    pub(crate) token_prices: Option<Arc<TokenGasPrices>>,
}

impl TokenPriceData {
    /// Builds a gas model from a block gas price and the derived token prices.
    ///
    /// With `None` for `token_prices` every cost is zero and the optimizer ranks on gross output.
    /// defibot instead falls back to a `DEFAULT_GAS_PRICE` of `1e-6`
    /// (`defibot/solver/models.py:29`), a constant in human units of whatever the buy token
    /// happens to be, which means something different for every token.
    pub(crate) fn new(gas_price_wei: BigUint, token_prices: Option<Arc<TokenGasPrices>>) -> Self {
        Self { gas_price_wei, token_prices }
    }

    /// Cost of `gas` gas units in on-chain units of `token`, or zero when no price is known.
    ///
    /// `token` is the alternative's *own* buy token, which is the order's buy token only at the
    /// branch level: a hop's alternatives produce the hop's output token, and a tail-grouped
    /// branch's sequences produce the token feeding its shared hop.
    pub(crate) fn cost_in_token(&self, gas: &BigUint, token: &Address) -> BigUint {
        let Some(price) = self
            .token_prices
            .as_ref()
            .and_then(|prices| prices.get(token))
        else {
            return BigUint::zero();
        };
        let Price { numerator, denominator } = price;
        if denominator.is_zero() {
            return BigUint::zero();
        }
        gas * &self.gas_price_wei * numerator / denominator
    }
}

/// A token path through the routing graph together with the pool used at each leg.
///
/// Owns its ids so nothing downstream carries the graph's lifetime.
pub(crate) struct DirectPath {
    /// Token addresses visited; one longer than [`DirectPath::components`].
    pub(crate) tokens: Vec<Address>,
    /// Component traded at each leg.
    pub(crate) components: Vec<ComponentId>,
}

/// An exact split fraction with a denominator of at most [`SPLIT_PRECISION`].
///
/// Equivalent of `fractions.Fraction(...).limit_denominator(SPLIT_PRECISION)`, the type defibot
/// stores in `SplitRoute.splits` (`routes/parallel.py:266-275`). Float splits are deliberately
/// avoided: a split is multiplied by a sell amount of up to ~10^30 and float drift would corrupt
/// the resulting integer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Fraction(BigRational);

impl Fraction {
    /// The zero split.
    pub(crate) fn zero() -> Self {
        Self(BigRational::zero())
    }

    /// The unit split — the whole amount.
    pub(crate) fn one() -> Self {
        Self(BigRational::one())
    }

    /// Builds a split from a rational, limiting its denominator to [`SPLIT_PRECISION`].
    pub(crate) fn new(value: BigRational) -> Self {
        Self(limit_denominator(&value))
    }

    /// Builds a split from `numerator / denominator`.
    ///
    /// Returns `None` when `denominator` is zero.
    ///
    /// Splits reach production code only through [`Fraction::new`] and [`Fraction::zero`]; this and
    /// [`Fraction::from_f64`] exist so fixtures can write the exact ratios defibot's tests use.
    #[cfg(test)]
    pub(crate) fn from_ratio(numerator: i64, denominator: i64) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        Some(Self::new(BigRational::new(BigInt::from(numerator), BigInt::from(denominator))))
    }

    /// Builds a split from a float.
    ///
    /// Returns `None` for NaN and infinities, which have no rational representation. See
    /// [`Fraction::from_ratio`] for why this is test-only.
    #[cfg(test)]
    pub(crate) fn from_f64(value: f64) -> Option<Self> {
        BigRational::from_float(value).map(Self::new)
    }

    /// The underlying exact rational.
    pub(crate) fn as_ratio(&self) -> &BigRational {
        &self.0
    }

    /// Lossy conversion for price arithmetic, which is `f64` throughout.
    ///
    /// Computed as a ratio of the two limbs so an out-of-range numerator saturates to infinity
    /// instead of panicking on a worker thread.
    pub(crate) fn to_f64(&self) -> f64 {
        let numerator = self
            .0
            .numer()
            .to_f64()
            .unwrap_or(f64::INFINITY);
        let denominator = self.0.denom().to_f64().unwrap_or(1.0);
        numerator / denominator
    }

    /// Whether this split routes nothing.
    pub(crate) fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// Applies the split to an on-chain amount, rounding down.
    ///
    /// Negative splits are meaningless for routing and yield zero rather than an error, matching
    /// the way defibot lets a zero split fall through to a zero-amount sell.
    pub(crate) fn apply(&self, amount: &BigUint) -> BigUint {
        if self.0.numer().is_negative() {
            return BigUint::zero();
        }
        let scaled = BigInt::from(amount.clone()) * self.0.numer() / self.0.denom();
        scaled
            .to_biguint()
            .unwrap_or_else(BigUint::zero)
    }
}

/// Nearest rational to `value` whose denominator is at most [`SPLIT_PRECISION`].
///
/// Continued-fraction expansion, the same algorithm CPython's `Fraction.limit_denominator` uses.
/// Only the magnitude is expanded so that truncating `BigInt` division behaves like the floor
/// division CPython relies on; the sign is reapplied at the end.
fn limit_denominator(value: &BigRational) -> BigRational {
    let max_denominator = BigInt::from(SPLIT_PRECISION);
    if value.denom() <= &max_denominator {
        return value.clone();
    }

    let negative = value.numer().is_negative();
    let target = if negative { -value.clone() } else { value.clone() };

    let (mut prev_numer, mut prev_denom) = (BigInt::zero(), BigInt::one());
    let (mut numer, mut denom) = (BigInt::one(), BigInt::zero());
    let mut remainder_numer = target.numer().clone();
    let mut remainder_denom = target.denom().clone();

    while !remainder_denom.is_zero() {
        let quotient = &remainder_numer / &remainder_denom;
        let next_denom = &prev_denom + &quotient * &denom;
        if next_denom > max_denominator {
            break;
        }
        let next_numer = &prev_numer + &quotient * &numer;
        prev_numer = numer;
        prev_denom = denom;
        numer = next_numer;
        denom = next_denom;
        let next_remainder_denom = &remainder_numer - &quotient * &remainder_denom;
        remainder_numer = remainder_denom;
        remainder_denom = next_remainder_denom;
    }

    // Two candidates bracket the target: the last convergent and the best mediant that still fits
    // under the denominator bound. Pick whichever lies closer, preferring the convergent on a tie.
    let steps = (&max_denominator - &prev_denom) / &denom;
    let lower = BigRational::new(&prev_numer + &steps * &numer, &prev_denom + &steps * &denom);
    let upper = BigRational::new(numer, denom);
    let closest = if (&upper - &target).abs() <= (&lower - &target).abs() { upper } else { lower };

    if negative {
        -closest
    } else {
        closest
    }
}
