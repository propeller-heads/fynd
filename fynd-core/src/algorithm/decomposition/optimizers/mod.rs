//! Split optimizers for the decomposition algorithm.
//!
//! Port of `defibot/solver/order_solver/decomposition/optimizers/`. An optimizer takes a set of
//! parallel alternatives, a sell amount and gas prices, and decides how much of the amount each
//! alternative should carry.
//!
//! # One optimizer, every level
//!
//! defibot's optimizers accept `list[FractalRoute]`, so the same code splits a hop's pools, a
//! grouped branch's tails and a solution's branches. This port needs no trait to match that,
//! because every split in the structure hands its amount to the same type: a
//! [`ParallelRoute`](super::components::ParallelRoute)'s alternatives are [`SplitKind`]s, and so
//! are the whole solution's, since the solution *is* a `ParallelRoute`.
//!
//! So the optimizers take `&mut [Route]` and nothing else. They sell trial amounts on the
//! alternatives and read the realised amounts back off them; that is the entire interface, and it
//! is [`SplitKind`]'s own. Which level is being split is not something an optimizer can tell, or
//! needs to.

pub(crate) mod equal_start_v2;
pub(crate) mod frank_wolfe;
pub(crate) mod pair_comparison;

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use tracing::debug;

use crate::algorithm::decomposition::{
    components::{DecompositionError, SplitKind},
    models::{Fraction, TokenPriceData},
    optimizers::{
        equal_start_v2::split_equal_start_v2, frank_wolfe::split_by_frank_wolfe,
        pair_comparison::split_by_pair_comparison,
    },
    RankingMetric,
};
// ===================== SplitOptimizer =====================

/// The result of splitting one sell amount over a set of alternatives.
///
/// defibot returns a bare `(sold, bought, splits)` tuple (`optimizers/interface.py:15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitSolution {
    /// Amount the alternatives together consumed. May be less than the requested amount.
    pub(crate) sold: BigUint,
    /// Amount the alternatives together produced.
    pub(crate) bought: BigUint,
    /// One split per alternative, in the order they were passed in.
    ///
    /// **These need not sum to one.** A shortfall means the alternatives could not absorb the
    /// whole sell amount (`optimizers/interface.py:26-31`); it is deliberately not normalised
    /// away because callers read the shortfall to decide whether the order is fillable.
    pub(crate) splits: Vec<Fraction>,
}

// ===================== Shared helpers =====================

/// Sells `sell_amount`, shrinking the amount by 10% on every recoverable failure until it succeeds
/// or reaches zero.
///
/// Port of `decrease_until_sell` (`defibot/solver/order_solver/decomposition/utils.py:74-105`).
/// Concentrated-liquidity pools routinely refuse a size they cannot fill; backing off finds the
/// largest size they will take instead of dropping the pool.
///
/// Returns `(0, 0)` when nothing could be sold. Note that this deliberately does *not* reset the
/// route: defibot's failed `sell` calls leave the previous sell's amounts in place, and the
/// optimizers read those amounts back.
///
/// `sell` is the only thing this needs of the level it backs off, so it takes that one operation
/// rather than a trait: the four levels that back off are four different types, and each already
/// has an inherent `sell`.
///
/// # Errors
///
/// Propagates any failure that is not [`DecompositionError::is_recoverable`] — a structural problem
/// will not go away by selling less.
pub(crate) fn decrease_until_sell(
    sell_amount: &BigUint,
    mut sell: impl FnMut(&BigUint) -> Result<(BigUint, BigUint), DecompositionError>,
) -> Result<(BigUint, BigUint), DecompositionError> {
    if sell_amount.is_zero() {
        return sell(&BigUint::zero());
    }

    let mut amount = sell_amount.clone();
    let mut rounds = 0usize;
    while !amount.is_zero() {
        match sell(&amount) {
            Ok(result) => {
                if rounds > 0 {
                    debug!(
                        requested = %sell_amount,
                        settled = %amount,
                        rounds,
                        "decrease_until_sell backed off before the sell succeeded"
                    );
                }
                return Ok(result);
            }
            Err(DecompositionError::SellAmountLimit { limit, token, pools }) => {
                // A multi-hop route casts an inner limit back through spot prices, so the reported
                // limit can come out above the amount we asked for. Falling back to backing off the
                // request keeps the sequence strictly decreasing, which is what terminates the
                // loop.
                let from_limit = back_off(&limit);
                debug!(
                    asked = %amount,
                    reported_limit = %limit,
                    %token,
                    ?pools,
                    "sell refused by a reported limit"
                );
                amount = if from_limit < amount { from_limit } else { back_off(&amount) };
            }
            Err(error) if error.is_recoverable() => {
                debug!(asked = %amount, %error, "sell failed in simulation; backing off 10%");
                amount = back_off(&amount);
            }
            Err(error) => return Err(error),
        }
        rounds += 1;
    }

    Ok((BigUint::zero(), BigUint::zero()))
}

/// Shrinks a sell amount by 10% (`utils.py:77`).
///
/// Integer floor division is what makes [`decrease_until_sell`] terminate: every amount below ten
/// maps strictly downwards and one maps to zero, so the sequence always reaches zero rather than
/// converging on a positive value.
fn back_off(amount: &BigUint) -> BigUint {
    amount * 9u8 / 10u8
}

/// `floor(amount * ratio)`, with negative ratios yielding zero.
///
/// Kept separate from [`Fraction::apply`] because the pair search walks a grid of exact rationals
/// whose denominators exceed `SPLIT_PRECISION`; rounding them to a split would move the grid.
fn scale(amount: &BigUint, ratio: &BigRational) -> BigUint {
    if ratio.numer().is_negative() {
        return BigUint::zero();
    }
    let scaled = BigInt::from(amount.clone()) * ratio.numer() / ratio.denom();
    scaled
        .to_biguint()
        .unwrap_or_else(BigUint::zero)
}

/// `numerator / denominator` as a split, or zero when `denominator` is zero.
///
/// defibot builds these as `Fraction(route.sell_amount, sell_amount)` and raises
/// `ZeroDivisionError` on a zero sell amount (`optimizers/pair_comparison.py:154`, `:175`, `:180`).
/// A zero denominator reaches that line whenever the pair search leaves both routes unable to sell
/// anything, so it yields a zero split here instead of a panic.
pub(crate) fn split_of(numerator: &BigUint, denominator: &BigUint) -> Fraction {
    if denominator.is_zero() {
        return Fraction::zero();
    }
    Fraction::new(BigRational::new(
        BigInt::from(numerator.clone()),
        BigInt::from(denominator.clone()),
    ))
}

/// An on-chain amount in human units.
///
/// Exact rational division before the `f64` conversion, so a 30-digit amount does not lose its
/// leading digits on the way.
fn to_human(amount: &BigUint, decimals: u32) -> f64 {
    let scaled = BigRational::new(BigInt::from(amount.clone()), BigInt::from(10u8).pow(decimals));
    scaled.to_f64().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

    use super::*;
    use crate::{
        algorithm::{
            decomposition::components::{Pool, SellLimitKind},
            test_utils::{token, ConstantProductSim},
        },
        derived::types::TokenGasPrices,
    };

    fn pool(id: &str, reserve_0: u64, reserve_1: u64) -> Pool {
        Pool::new(
            id.to_string(),
            Arc::new(token(0x0A, "A")),
            Arc::new(token(0x0B, "B")),
            SellLimitKind::Enforced,
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve_0),
                reserve_1: BigUint::from(reserve_1),
                gas: 50_000,
            }),
            None,
        )
    }

    #[test]
    fn test_decrease_until_sell_backs_off_to_the_pool_limit() {
        // ConstantProductSim caps a sell at half its input reserve.
        let mut pool = pool("p", 1_000, 1_000);

        let (bought, _) = decrease_until_sell(&BigUint::from(900u32), |amount| pool.sell(amount))
            .expect("back-off finds a sellable amount");

        assert!(pool.sell_amount() <= &BigUint::from(500u32));
        assert!(!bought.is_zero());
    }

    #[test]
    fn test_decrease_until_sell_reaches_zero_when_nothing_sells() {
        // A pool with a one-unit reserve caps sells at zero, so every back-off fails.
        let mut pool = pool("p", 1, 1);

        let (bought, gas) =
            decrease_until_sell(&BigUint::from(1_000u32), |amount| pool.sell(amount))
                .expect("exhausting the back-off is not an error");

        assert!(bought.is_zero());
        assert!(gas.is_zero());
    }

    #[test]
    fn test_decrease_until_sell_zero_resets_the_route() {
        let mut pool = pool("p", 1_000_000, 1_000_000);
        decrease_until_sell(&BigUint::from(1_000u32), |amount| pool.sell(amount)).expect("sells");

        decrease_until_sell(&BigUint::zero(), |amount| pool.sell(amount))
            .expect("zero always succeeds");

        assert!(pool.sell_amount().is_zero());
        assert!(pool.buy_amount().is_zero());
    }

    #[test]
    fn test_back_off_always_reaches_zero() {
        let mut amount = BigUint::from(1_000_000u32);
        for _ in 0..200 {
            amount = back_off(&amount);
        }

        assert!(amount.is_zero());
    }

    #[test]
    fn test_gas_prices_without_token_prices_are_free() {
        let gas_price_wei = BigUint::from(1_000u32);
        let prices = TokenPriceData::new(gas_price_wei.clone(), None);

        assert!(prices
            .cost_in_token(&BigUint::from(100_000u32), &token(0x0B, "B").address)
            .is_zero());
    }

    #[test]
    fn test_gas_prices_convert_gas_to_token_units() {
        let buy_token = token(0x0B, "B");
        let mut token_prices: TokenGasPrices = FxHashMap::default();
        token_prices
            .insert(buy_token.address.clone(), Price::new(BigUint::from(3u8), BigUint::from(2u8)));
        let gas_price_wei = BigUint::from(10u8);
        let prices =
            TokenPriceData::new(gas_price_wei.clone(), Some(Arc::new(token_prices.clone())));

        // 100 gas * 10 wei/gas * 3/2 token-per-wei.
        assert_eq!(
            prices.cost_in_token(&BigUint::from(100u8), &buy_token.address),
            BigUint::from(1_500u32)
        );
    }

    #[test]
    fn test_split_of_zero_denominator() {
        assert_eq!(split_of(&BigUint::from(5u8), &BigUint::zero()), Fraction::zero());
    }

    #[test]
    fn test_to_human_keeps_precision_for_large_amounts() {
        let amount = BigUint::from(10u8).pow(18) * BigUint::from(1_234u32);

        assert!((to_human(&amount, 18) - 1_234.0).abs() < 1e-9);
    }
}

/// A split optimizer: how an amount is divided between parallel alternatives.
///
/// The first two are defibot's `solver.order_solver.decomposition.optimizer`; the third is not in
/// defibot. Each one's module says what it does and what it measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitOptimizer {
    /// Pairwise line search (`optimizers/pair_comparison.rs`).
    PairComparison,
    /// Equal-start gradient walk (`optimizers/equal_start_v2.rs`).
    EqualStartV2 { equalize: RankingMetric },
    /// Frank-Wolfe line search (`optimizers/frank_wolfe.rs`).
    FrankWolfe,
}

impl SplitOptimizer {
    /// Divides `sell_amount` between `routes`.
    ///
    /// # Errors
    ///
    /// Whatever the trial sells raise once backing off cannot recover.
    pub(crate) fn split(
        &self,
        routes: &mut [SplitKind],
        sell_amount: &BigUint,
        gas_prices: &TokenPriceData,
    ) -> Result<SplitSolution, DecompositionError> {
        match self {
            SplitOptimizer::PairComparison => {
                split_by_pair_comparison(routes, sell_amount, gas_prices)
            }
            SplitOptimizer::EqualStartV2 { equalize } => {
                split_equal_start_v2(*equalize, routes, sell_amount, gas_prices)
            }
            SplitOptimizer::FrankWolfe => split_by_frank_wolfe(routes, sell_amount, gas_prices),
        }
    }
}

/// Which optimizer runs at which level of the solve.
///
/// A solve splits twice. The outer split hands the order to the branches; the inner splits hand a
/// branch's share to its sequences, and a hop's share to its pools. They do not have to use the
/// same optimizer, and on the recorded fixture they should not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOptimizerConfig {
    /// Splits the order across the graph's branches.
    pub outer: SplitOptimizer,
    /// Splits inside a branch: its sequences, and the pools of a hop.
    pub inner: SplitOptimizer,
}

impl Default for SplitOptimizerConfig {
    /// Pairwise over the branches, Frank-Wolfe inside them.
    fn default() -> Self {
        Self { outer: SplitOptimizer::PairComparison, inner: SplitOptimizer::FrankWolfe }
    }
}
