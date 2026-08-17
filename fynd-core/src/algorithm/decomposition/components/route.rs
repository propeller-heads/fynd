//! One level of a solution: a split, a chain, or a pool.
//!
//! Replaces the three types that made up the recursion. defibot's `FractalRoute` is the same three
//! shapes (`routes/simple.py`, `routes/parallel.py`, `routes/sequential.py`); the port had them as
//! separate structs, which meant writing each of the nine composed attributes three times and gave
//! the optimizers nothing common to be generic over.
//!
//! Each shape keeps its own struct and its own file — [`SplitRoute`](super::split::ParallelRoute)
//! in `hop.rs`, [`SequenceRoute`](super::sequence::SequenceRoute) in `sequence.rs`,
//! [`PoolRef`](super::pool::Pool) in `pool.rs`. This file holds only the enum and the dispatch,
//! so anything that belongs to one shape alone lives on that shape and a caller holding it never
//! matches for something that cannot fail.
//!
//! # Bounded depth without a bounded type
//!
//! defibot's tree nests without limit, which is what the port removed. Nothing here brings that
//! back: `graph_build` is the only thing that builds a route and it produces exactly one shape — a
//! split over branches, each a chain whose hops are splits over pools. What is given up against
//! three separate structs is that the compiler no longer proves a split cannot contain a split;
//! [`ParallelRoute::new`] rejects it at construction instead, so an invalid level cannot exist.

use std::sync::Arc;

use num_bigint::BigUint;
use tycho_simulation::tycho_core::models::token::Token;

use crate::{
    algorithm::decomposition::components::{
        pool::Pool, sequence::SequenceRoute, split::ParallelRoute, ComponentId, DecompositionError,
    },
    derived::types::TokenGasPrices,
};

/// One alternative inside a [`ParallelRoute`]: a pool, or a chain of hops.
///
/// There is no parallel variant, and that is a guarantee rather than an omission. A `Route` only
/// ever lives in `SplitRoute::children`, so a split of splits is the one shape this enum could
/// express that nothing should build — leaving it out makes that unrepresentable instead of
/// rejected at construction.
pub(crate) enum SplitKind {
    /// Hops in series: one tail of a grouped branch.
    Sequence(SequenceRoute),
    /// One pool traded in one direction.
    Direct(Pool),
}

impl SplitKind {
    /// Wraps a pool as a leaf.
    pub(crate) fn pool(pool: Pool) -> Self {
        Self::Direct(pool)
    }

    /// Builds a chain over `hops`.
    ///
    /// # Errors
    ///
    /// Whatever [`SequenceRoute::new`] raises.
    pub(crate) fn sequence(hops: Vec<ParallelRoute>) -> Result<Self, DecompositionError> {
        SequenceRoute::new(hops).map(Self::Sequence)
    }

    // ===================== Shared by all three =====================

    /// Token this level consumes.
    pub(crate) fn sell_token(&self) -> &Token {
        match self {
            Self::Sequence(sequence) => sequence.sell_token(),
            Self::Direct(pool) => pool.token_in(),
        }
    }

    /// Token this level produces.
    pub(crate) fn buy_token(&self) -> &Token {
        match self {
            Self::Sequence(sequence) => sequence.buy_token(),
            Self::Direct(pool) => pool.token_out(),
        }
    }

    /// [`SplitKind::sell_token`] as a shared handle, for callers that keep it.
    #[cfg(test)]
    pub(crate) fn sell_token_shared(&self) -> Arc<Token> {
        match self {
            Self::Sequence(sequence) => sequence.sell_token_shared(),
            Self::Direct(pool) => pool.token_in_shared(),
        }
    }

    /// [`SplitKind::buy_token`] as a shared handle.
    #[cfg(test)]
    pub(crate) fn buy_token_shared(&self) -> Arc<Token> {
        match self {
            Self::Sequence(sequence) => sequence.buy_token_shared(),
            Self::Direct(pool) => pool.token_out_shared(),
        }
    }

    /// Whether this level is ready to be sold on. A pool always is — there is nothing below it to
    /// solve (`routes/simple.py:46-47`).
    pub(crate) fn solved(&self) -> bool {
        match self {
            Self::Sequence(sequence) => sequence.solved(),
            Self::Direct(_) => true,
        }
    }

    /// Amount of [`SplitKind::sell_token`] the last sell consumed.
    pub(crate) fn sell_amount(&self) -> &BigUint {
        match self {
            Self::Sequence(sequence) => sequence.sell_amount(),
            Self::Direct(pool) => pool.sell_amount(),
        }
    }

    /// Amount of [`SplitKind::buy_token`] the last sell produced.
    pub(crate) fn buy_amount(&self) -> &BigUint {
        match self {
            Self::Sequence(sequence) => sequence.buy_amount(),
            Self::Direct(pool) => pool.buy_amount(),
        }
    }

    /// Spot price of this level, before fees.
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        match self {
            Self::Sequence(sequence) => sequence.route_price(),
            Self::Direct(pool) => pool.route_price(),
        }
    }

    /// Price net of fees at the pre-trade state.
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        match self {
            Self::Sequence(sequence) => sequence.marginal_price(),
            Self::Direct(pool) => pool.marginal_price(),
        }
    }

    /// Price net of fees at the state the last sell left behind.
    ///
    /// `None` when this level was not sold on, which propagates up from whichever level below has
    /// no post-trade state.
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        match self {
            Self::Sequence(sequence) => sequence.new_marginal_price(),
            Self::Direct(pool) => pool.new_marginal_price(),
        }
    }

    /// Trading fee as a fraction of the input.
    pub(crate) fn fee(&self) -> f64 {
        match self {
            Self::Sequence(sequence) => sequence.fee(),
            Self::Direct(pool) => pool.fee(),
        }
    }

    /// Gas of every pool below this level, whatever the splits.
    pub(crate) fn gas(&self) -> BigUint {
        match self {
            Self::Sequence(sequence) => sequence.gas(),
            Self::Direct(pool) => pool.gas().clone(),
        }
    }

    /// Gas of only the pools this level's splits activate.
    ///
    /// See [`ParallelRoute::minimum_gas`] for why this is the only gas an optimizer may charge.
    pub(crate) fn minimum_gas(&self) -> BigUint {
        match self {
            Self::Sequence(sequence) => sequence.minimum_gas(),
            Self::Direct(pool) => pool.gas().clone(),
        }
    }

    /// Liquidity depth proxy, in human units of [`SplitKind::sell_token`].
    pub(crate) fn inertia(&self) -> f64 {
        match self {
            Self::Sequence(sequence) => sequence.inertia(),
            Self::Direct(pool) => pool.inertia(),
        }
    }

    /// Ranking score: `inertia * (1 - fee) * route_price`.
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        match self {
            Self::Sequence(sequence) => sequence.weight(),
            Self::Direct(pool) => pool.weight(),
        }
    }

    /// Price the last sell achieved, in human units. Gas is not accounted for.
    pub(crate) fn executed_price(&self) -> f64 {
        match self {
            Self::Sequence(sequence) => sequence.executed_price(),
            Self::Direct(pool) => pool.executed_price(),
        }
    }

    /// Sells `amount` through this level.
    ///
    /// # Errors
    ///
    /// Whatever the shape raises. A [`DecompositionError::SellAmountLimit`] is always denominated
    /// in [`SplitKind::sell_token`], so a limit hit at an intermediate token is cast back first.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        match self {
            Self::Sequence(sequence) => sequence.sell(amount),
            Self::Direct(pool) => pool.sell(amount),
        }
    }

    /// Largest amount of [`SplitKind::sell_token`] this level can absorb, and the pools that bound
    /// it.
    ///
    /// # Errors
    ///
    /// Whatever the pools raise while reporting their own limits.
    pub(crate) fn sell_amount_limit(
        &mut self,
    ) -> Result<(BigUint, Vec<ComponentId>), DecompositionError> {
        match self {
            Self::Sequence(sequence) => sequence.sell_amount_limit(),
            Self::Direct(pool) => {
                let limit = pool.sell_amount_limit()?;
                Ok((limit, vec![pool.component_id().clone()]))
            }
        }
    }

    /// Hands the derived mid-prices to every chain below this level.
    pub(crate) fn set_prices(&mut self, prices: Arc<TokenGasPrices>) {
        match self {
            Self::Sequence(sequence) => sequence.set_prices(prices),
            Self::Direct(_) => {}
        }
    }

    /// Drops every memoised limit and swap below this level.
    pub(crate) fn invalidate(&mut self) {
        match self {
            Self::Sequence(sequence) => sequence.invalidate(),
            Self::Direct(pool) => pool.invalidate(),
        }
    }

    // ===================== Walking =====================

    /// Every hop below this level, in flow order. See [`SequenceRoute::all_hops`].
    pub(crate) fn all_hops(&self) -> Vec<&ParallelRoute> {
        let mut found = Vec::new();
        self.collect_hops(&mut found);
        found
    }

    /// Marks every split below this level unsolved. See [`SequenceRoute::reset_splits`].
    pub(crate) fn reset_splits(&mut self) {
        match self {
            Self::Direct(_) => {}
            Self::Sequence(sequence) => sequence.reset_splits(),
        }
    }

    /// This level's token path rendered from symbols. A pool is its own pair.
    pub(crate) fn token_path_label(&self) -> String {
        match self {
            Self::Direct(pool) => {
                format!("{}->{}", pool.token_in().symbol, pool.token_out().symbol)
            }
            Self::Sequence(sequence) => sequence.token_path_label(),
        }
    }

    /// Adds every hop below this level, in flow order. See [`SequenceRoute::all_hops`].
    pub(super) fn collect_hops<'a>(&'a self, found: &mut Vec<&'a ParallelRoute>) {
        match self {
            Self::Direct(_) => {}
            Self::Sequence(sequence) => sequence.collect_hops(found),
        }
    }

    /// Runs `visit` on every pool below this level, depth first.
    ///
    /// The mutable counterpart of walking [`SequenceRoute::all_hops`], which borrowing rules do not
    /// allow to hand out `&mut` hops one at a time.
    pub(crate) fn for_each_pool_mut(&mut self, visit: &mut impl FnMut(&mut Pool)) {
        match self {
            Self::Direct(pool) => visit(pool),
            Self::Sequence(sequence) => sequence.for_each_pool_mut(visit),
        }
    }
}
