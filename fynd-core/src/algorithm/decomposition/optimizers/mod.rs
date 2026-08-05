//! Split optimizers for the decomposition algorithm.
//!
//! Port of `defibot/solver/order_solver/decomposition/optimizers/`. An optimizer takes a set of
//! parallel alternatives, a sell amount and gas prices, and decides how much of the amount each
//! alternative should carry.
//!
//! # Working at three levels
//!
//! defibot's optimizers accept `list[FractalRoute]`, so the same code splits a hop's pools, a
//! branch's tails and a solution's parallel branches — the recursion made all three look
//! identical. This port replaced the recursion with a fixed structure
//! ([`SolutionGraph`](super::components::SolutionGraph) holds [`Branch`]es, a [`Branch`] holds
//! [`SequentialRoute`] tails, a [`Hop`] holds [`PoolRef`]s), so the polymorphism has to come from
//! somewhere else.
//!
//! [`Sellable`] is that somewhere: the smallest interface an optimizer actually uses — sell on it,
//! read back what was sold and bought, and read the ranking quantities. [`Branch`] implements it
//! for the outer split and [`SequentialRoute`] for a branch's inner split over its tails, while
//! [`HopPool`] binds a [`PoolRef`] to the token pair its enclosing [`Hop`] trades so that a pool
//! satisfies it too. Optimizers are generic over the trait and never see a route tree.

pub(crate) mod equal_start_v2;
pub(crate) mod frank_wolfe;
pub(crate) mod pair_comparison;

use std::sync::Arc;

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use tracing::debug;
use tycho_simulation::tycho_core::{
    models::{token::Token, Address},
    simulation::protocol_sim::Price,
};

use crate::{
    algorithm::decomposition::components::{
        Branch, DecompositionError, Fraction, Hop, PoolRef, SequentialRoute, SolutionGraph,
    },
    derived::types::TokenGasPrices,
};

// ===================== Sellable =====================

/// Something an optimizer can sell on and rank against its peers.
///
/// The union of what defibot's optimizers read off a `FractalRoute`, and nothing more. Every method
/// except [`Sellable::sell`] reports the state left behind by the last sell, which is how the
/// optimizers communicate: they sell trial amounts and then read the realised amounts back.
pub(crate) trait Sellable {
    /// Token this alternative consumes.
    fn sell_token(&self) -> &Token;

    /// Token this alternative produces.
    fn buy_token(&self) -> &Token;

    /// Whether the alternative is ready to be sold on. An unsolved alternative is ranked last and
    /// never sold (`optimizers/pair_comparison.py:120-122`).
    fn solved(&self) -> bool;

    /// Amount of [`Sellable::sell_token`] the last sell consumed.
    fn sell_amount(&self) -> &BigUint;

    /// Amount of [`Sellable::buy_token`] the last sell produced.
    fn buy_amount(&self) -> &BigUint;

    /// Gas of only the components the alternative's splits activate.
    ///
    /// defibot's `minimum_gas` (`routes/parallel.py:281-286`, `routes/sequential.py:93-94`), and
    /// the quantity the pruning bound must charge. defibot's sibling `gas` sums every component
    /// regardless of split, so a component left holding gas from an earlier sell but ending on a
    /// zero split — exactly what a split search leaves behind — is counted by that one and not
    /// here. It is deliberately **not** part of this trait: the only quantity an optimizer may
    /// charge is this one, and offering the other would make the unsound choice reachable.
    ///
    /// **Charging too much gas here is unsound, charging too little is merely slow.** The pruning
    /// bound subtracts this from a zero-impact `marginal_price` to get the best price an
    /// alternative could conceivably reach, and drops it only when even that loses. Over-charging
    /// pushes that bound below the truth, so it stops being an upper bound and a genuinely useful
    /// alternative gets dropped before it is ever searched. Under-charging only leaves a hopeless
    /// alternative in the search, which costs time and nothing else.
    ///
    /// Fynd's `ProtocolSim` has no equivalent of defibot's static `minimum_swap_gas`
    /// (`routes/simple.py:256-257`), so this reports the *realised* gas of the activated
    /// components. Realised gas is at least the static floor, so the result is an upper bound by a
    /// little — safe in the direction that matters only because the filter above removes the large
    /// half of the discrepancy. Do not widen it back to a sum over every component.
    fn minimum_gas(&self) -> BigUint;

    /// Price net of fees at the current (zero-impact) state, in human units.
    fn marginal_price(&self) -> Result<f64, DecompositionError>;

    /// Price net of fees at the state the last sell left behind, in human units.
    ///
    /// `None` when the alternative has not been sold on, so there is no post-trade state to price.
    /// This is the quantity [`EqualStartV2`](equal_start_v2::EqualStartV2) equalises across
    /// alternatives: two alternatives whose *next* unit prices the same cannot be improved by
    /// moving flow between them.
    fn new_marginal_price(&self) -> Result<Option<f64>, DecompositionError>;

    /// Price the last sell actually achieved, in human units. Gas is not accounted for.
    fn executed_price(&self) -> f64;

    /// Sells `amount`, returning the bought amount and the gas used.
    ///
    /// Selling zero resets the alternative.
    ///
    /// # Errors
    ///
    /// Whatever the underlying structure raises; optimizers retry on
    /// [`DecompositionError::is_recoverable`] failures and propagate the rest.
    fn sell(&mut self, amount: &BigUint) -> Result<(BigUint, BigUint), DecompositionError>;
}

impl Sellable for SequentialRoute {
    fn sell_token(&self) -> &Token {
        SequentialRoute::sell_token(self)
    }

    fn buy_token(&self) -> &Token {
        SequentialRoute::buy_token(self)
    }

    fn solved(&self) -> bool {
        SequentialRoute::solved(self)
    }

    fn sell_amount(&self) -> &BigUint {
        SequentialRoute::sell_amount(self)
    }

    fn buy_amount(&self) -> &BigUint {
        SequentialRoute::buy_amount(self)
    }

    fn minimum_gas(&self) -> BigUint {
        SequentialRoute::minimum_gas(self)
    }

    fn marginal_price(&self) -> Result<f64, DecompositionError> {
        SequentialRoute::marginal_price(self)
    }

    fn new_marginal_price(&self) -> Result<Option<f64>, DecompositionError> {
        Ok(SequentialRoute::new_marginal_price(self))
    }

    fn executed_price(&self) -> f64 {
        SequentialRoute::executed_price(self)
    }

    fn sell(&mut self, amount: &BigUint) -> Result<(BigUint, BigUint), DecompositionError> {
        SequentialRoute::sell(self, amount)
    }
}

/// A branch is what the *outer* split is over: one shared first hop plus its parallel tails.
///
/// Its tails are [`SequentialRoute`]s, which implement this trait themselves, so the inner split
/// over a branch's tails runs through the same optimizers at the level below with no extra
/// adapter.
impl Sellable for Branch {
    fn sell_token(&self) -> &Token {
        Branch::sell_token(self)
    }

    fn buy_token(&self) -> &Token {
        Branch::buy_token(self)
    }

    fn solved(&self) -> bool {
        Branch::solved(self)
    }

    fn sell_amount(&self) -> &BigUint {
        Branch::sell_amount(self)
    }

    fn buy_amount(&self) -> &BigUint {
        Branch::buy_amount(self)
    }

    fn minimum_gas(&self) -> BigUint {
        Branch::minimum_gas(self)
    }

    fn marginal_price(&self) -> Result<f64, DecompositionError> {
        Branch::marginal_price(self)
    }

    fn new_marginal_price(&self) -> Result<Option<f64>, DecompositionError> {
        Ok(Branch::new_marginal_price(self))
    }

    fn executed_price(&self) -> f64 {
        Branch::executed_price(self)
    }

    fn sell(&mut self, amount: &BigUint) -> Result<(BigUint, BigUint), DecompositionError> {
        Branch::sell(self, amount)
    }
}

/// A hop is an alternative in its own right when the solver treats it as one unit — the
/// single-pool case of `recursive_solve_splits` (`order_solver.py:661-671`) backs a hop off through
/// [`decrease_until_sell`] exactly as it would any other route.
impl Sellable for Hop {
    fn sell_token(&self) -> &Token {
        self.token_in()
    }

    fn buy_token(&self) -> &Token {
        self.token_out()
    }

    fn solved(&self) -> bool {
        Hop::solved(self)
    }

    fn sell_amount(&self) -> &BigUint {
        Hop::sell_amount(self)
    }

    fn buy_amount(&self) -> &BigUint {
        Hop::buy_amount(self)
    }

    fn minimum_gas(&self) -> BigUint {
        Hop::minimum_gas(self)
    }

    fn marginal_price(&self) -> Result<f64, DecompositionError> {
        Hop::marginal_price(self)
    }

    fn new_marginal_price(&self) -> Result<Option<f64>, DecompositionError> {
        Ok(Hop::new_marginal_price(self))
    }

    fn executed_price(&self) -> f64 {
        Hop::executed_price(self)
    }

    fn sell(&mut self, amount: &BigUint) -> Result<(BigUint, BigUint), DecompositionError> {
        Hop::sell(self, amount)
    }
}

/// The whole graph is an alternative in its own right in the single-branch case of
/// `recursive_solve_splits` (`order_solver.py:661-671`), which backs the graph off as one unit.
impl Sellable for SolutionGraph {
    fn sell_token(&self) -> &Token {
        SolutionGraph::sell_token(self)
    }

    fn buy_token(&self) -> &Token {
        SolutionGraph::buy_token(self)
    }

    fn solved(&self) -> bool {
        SolutionGraph::solved(self)
    }

    fn sell_amount(&self) -> &BigUint {
        SolutionGraph::sell_amount(self)
    }

    fn buy_amount(&self) -> &BigUint {
        SolutionGraph::buy_amount(self)
    }

    fn minimum_gas(&self) -> BigUint {
        SolutionGraph::minimum_gas(self)
    }

    fn marginal_price(&self) -> Result<f64, DecompositionError> {
        SolutionGraph::marginal_price(self)
    }

    fn new_marginal_price(&self) -> Result<Option<f64>, DecompositionError> {
        Ok(SolutionGraph::new_marginal_price(self))
    }

    fn executed_price(&self) -> f64 {
        SolutionGraph::executed_price(self)
    }

    fn sell(&mut self, amount: &BigUint) -> Result<(BigUint, BigUint), DecompositionError> {
        SolutionGraph::sell(self, amount)
    }
}

/// A [`PoolRef`] bound to the token pair of the [`Hop`] that holds it.
///
/// A pool on its own does not know which direction it is being traded in — [`PoolRef`] takes the
/// token pair on every call — so it cannot implement [`Sellable`] directly. Binding the pair here
/// keeps the tokens in one place instead of storing them on every pool.
pub(crate) struct HopPool<'a> {
    pool: &'a mut PoolRef,
    token_in: Token,
    token_out: Token,
}

impl<'a> HopPool<'a> {
    /// Binds every pool of `hop` to the hop's token pair.
    ///
    /// The result is what an optimizer splits when solving a single leg.
    pub(crate) fn bind_all(hop: &'a mut Hop) -> Vec<HopPool<'a>> {
        let token_in = hop.token_in().clone();
        let token_out = hop.token_out().clone();
        hop.pools_mut()
            .iter_mut()
            .map(|pool| HopPool { pool, token_in: token_in.clone(), token_out: token_out.clone() })
            .collect()
    }
}

impl Sellable for HopPool<'_> {
    fn sell_token(&self) -> &Token {
        &self.token_in
    }

    fn buy_token(&self) -> &Token {
        &self.token_out
    }

    /// A single pool is always ready to trade — there is nothing below it to solve
    /// (`routes/simple.py:46-47`).
    fn solved(&self) -> bool {
        true
    }

    fn sell_amount(&self) -> &BigUint {
        self.pool.sell_amount()
    }

    fn buy_amount(&self) -> &BigUint {
        self.pool.buy_amount()
    }

    /// A single pool has no splits below it, so it is always the pool it activates.
    fn minimum_gas(&self) -> BigUint {
        self.pool.gas().clone()
    }

    fn marginal_price(&self) -> Result<f64, DecompositionError> {
        self.pool
            .marginal_price(&self.token_in, &self.token_out)
    }

    fn new_marginal_price(&self) -> Result<Option<f64>, DecompositionError> {
        Ok(self
            .pool
            .new_marginal_price(&self.token_in, &self.token_out))
    }

    fn executed_price(&self) -> f64 {
        self.pool
            .executed_price(&self.token_in, &self.token_out)
    }

    fn sell(&mut self, amount: &BigUint) -> Result<(BigUint, BigUint), DecompositionError> {
        self.pool
            .sell(amount, &self.token_in, &self.token_out)
    }
}

// ===================== Gas pricing =====================

/// Gas cost expressed in a token.
///
/// defibot passes `dict[symbol, Decimal]` holding, per token, the price of one gas unit denominated
/// in that token (`optimizers/interface.py:13`). Fynd splits the same quantity in two: the block's
/// gas price in wei, and [`TokenGasPrices`] mapping a token to its wei ratio.
#[derive(Clone)]
pub(crate) struct GasPrices {
    gas_price_wei: BigUint,
    token_prices: Option<Arc<TokenGasPrices>>,
}

impl GasPrices {
    /// Builds a gas model from a block gas price and the derived token prices.
    ///
    /// With `None` for `token_prices` every cost is zero and the optimizer ranks on gross output.
    /// defibot instead falls back to a `DEFAULT_GAS_PRICE` of `1e-6`
    /// (`defibot/solver/models.py:29`), a constant in human units of whatever the buy token
    /// happens to be, which means something different for every token.
    pub(crate) fn new(gas_price_wei: BigUint, token_prices: Option<Arc<TokenGasPrices>>) -> Self {
        Self { gas_price_wei, token_prices }
    }

    /// The derived prices this was built from, for the callers that price a whole route.
    pub(crate) fn token_prices(&self) -> Option<&Arc<TokenGasPrices>> {
        self.token_prices.as_ref()
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

/// Decides how a sell amount is divided between parallel alternatives.
///
/// Port of defibot's `SplitOptimizer` Protocol (`optimizers/interface.py:8-32`). defibot's
/// `**kwargs` channel is gone: the one keyword any caller passed was `initial_splits`, which
/// `split_by_pair_comparison` rejects at runtime (`optimizers/pair_comparison.py:34-38`). Leaving
/// it out of the signature turns that runtime rejection into a compile error.
///
/// The method is generic rather than taking `&mut [&mut dyn Sellable]`, so this trait is not
/// object-safe. There is one implementation; a second one is selected by matching on config, not by
/// boxing.
pub(crate) trait SplitOptimizerT {
    /// Splits `sell_amount` over `routes`, selling on them as it searches.
    ///
    /// `routes` is left holding the amounts of the chosen split, which is how callers recover the
    /// per-alternative sell and buy amounts.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] for a zero `sell_amount`, and any non-recoverable
    /// failure raised while selling.
    fn optimize<S: Sellable>(
        &self,
        routes: &mut [S],
        sell_amount: &BigUint,
        gas_prices: &GasPrices,
    ) -> Result<SplitSolution, DecompositionError>;
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
/// # Errors
///
/// Propagates any failure that is not [`DecompositionError::is_recoverable`] — a structural problem
/// will not go away by selling less.
pub(crate) fn decrease_until_sell<S: Sellable + ?Sized>(
    route: &mut S,
    sell_amount: &BigUint,
) -> Result<(BigUint, BigUint), DecompositionError> {
    if sell_amount.is_zero() {
        return route.sell(&BigUint::zero());
    }

    let mut amount = sell_amount.clone();
    let mut rounds = 0usize;
    while !amount.is_zero() {
        match route.sell(&amount) {
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

    use rustc_hash::FxHashMap;

    use super::*;
    use crate::algorithm::{
        decomposition::components::{Hop, SellLimitKind},
        test_utils::{token, ConstantProductSim},
    };

    fn pool(id: &str, reserve_0: u64, reserve_1: u64) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve_0),
                reserve_1: BigUint::from(reserve_1),
                gas: 50_000,
            }),
            None,
        )
    }

    fn hop(pools: Vec<PoolRef>) -> Hop {
        Hop::new(token(0x0A, "A"), token(0x0B, "B"), pools).expect("hop has pools")
    }

    #[test]
    fn test_decrease_until_sell_backs_off_to_the_pool_limit() {
        // ConstantProductSim caps a sell at half its input reserve.
        let mut hop = hop(vec![pool("p", 1_000, 1_000)]);
        let mut legs = HopPool::bind_all(&mut hop);

        let (bought, _) = decrease_until_sell(&mut legs[0], &BigUint::from(900u32))
            .expect("back-off finds a sellable amount");

        assert!(legs[0].sell_amount() <= &BigUint::from(500u32));
        assert!(!bought.is_zero());
    }

    #[test]
    fn test_decrease_until_sell_reaches_zero_when_nothing_sells() {
        // A pool with a one-unit reserve caps sells at zero, so every back-off fails.
        let mut hop = hop(vec![pool("p", 1, 1)]);
        let mut legs = HopPool::bind_all(&mut hop);

        let (bought, gas) = decrease_until_sell(&mut legs[0], &BigUint::from(1_000u32))
            .expect("exhausting the back-off is not an error");

        assert!(bought.is_zero());
        assert!(gas.is_zero());
    }

    #[test]
    fn test_decrease_until_sell_zero_resets_the_route() {
        let mut hop = hop(vec![pool("p", 1_000_000, 1_000_000)]);
        let mut legs = HopPool::bind_all(&mut hop);
        decrease_until_sell(&mut legs[0], &BigUint::from(1_000u32)).expect("sells");

        decrease_until_sell(&mut legs[0], &BigUint::zero()).expect("zero always succeeds");

        assert!(legs[0].sell_amount().is_zero());
        assert!(legs[0].buy_amount().is_zero());
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
        let prices = GasPrices::new(gas_price_wei.clone(), None);

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
        let prices = GasPrices::new(gas_price_wei.clone(), Some(Arc::new(token_prices.clone())));

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
