//! Alternatives in parallel, and the rules that compose them.

use num_bigint::BigUint;
use num_rational::BigRational;
use num_traits::{One, Zero};
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::components::*;

// ===================== SplitRoute =====================

/// Alternatives in parallel, and how the amount is divided between them.
///
/// Equivalent of defibot's `ParallelRoute` (`routes/parallel.py`). Two levels of a solution have
/// this shape: the pools of one leg, and the branches or tails a split search hands an order to.
///
/// Unsolved — empty `splits` — every composed attribute is an estimate over all the alternatives;
/// once solved they become split-weighted.
pub(crate) struct ParallelRoute {
    children: Vec<Route>,
    splits: Vec<Fraction>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    /// Summed sell limit and the pools responsible, memoised until [`ParallelRoute::invalidate`].
    limit_cache: Option<(BigUint, Vec<ComponentId>)>,
}

impl ParallelRoute {
    /// Builds an unsolved split over `children`.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when `children` is empty — every rule here averages
    /// or maximises over them and has no meaning for none — or when they do not all trade the same
    /// pair, which is what makes summing their outputs valid (`routes/parallel.py:289-292`).
    ///
    /// A split of splits needs no check: [`Route`] cannot express one.
    pub(crate) fn new(children: Vec<Route>) -> Result<Self, DecompositionError> {
        let Some(first) = children.first() else {
            return Err(DecompositionError::InvalidStructure {
                reason: "a split needs at least one alternative".to_string(),
            });
        };
        let (sell_token, buy_token) = (first.sell_token().clone(), first.buy_token().clone());
        for child in &children {
            if child.sell_token().address != sell_token.address ||
                child.buy_token().address != buy_token.address
            {
                return Err(DecompositionError::InvalidStructure {
                    reason: format!(
                        "a split's alternatives must share both endpoints; {} -> {} does not match \
                         {} -> {}",
                        child.sell_token().address,
                        child.buy_token().address,
                        sell_token.address,
                        buy_token.address,
                    ),
                });
            }
        }
        Ok(Self {
            children,
            splits: Vec::new(),
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
        })
    }

    /// The alternatives this level divides between.
    pub(crate) fn children(&self) -> &[Route] {
        &self.children
    }

    /// The alternatives, for an optimizer to sell trial amounts on.
    pub(crate) fn children_mut(&mut self) -> &mut [Route] {
        &mut self.children
    }

    /// How the amount is divided, empty while unsolved.
    pub(crate) fn splits(&self) -> &[Fraction] {
        &self.splits
    }

    /// Sets how the amount is divided. An empty vector marks the level unsolved again.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when the vector does not have one entry per
    /// alternative.
    pub(crate) fn set_splits(&mut self, splits: Vec<Fraction>) -> Result<(), DecompositionError> {
        if !splits.is_empty() && splits.len() != self.children.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "a split over {} alternatives got {} splits",
                    self.children.len(),
                    splits.len()
                ),
            });
        }
        self.splits = splits;
        self.limit_cache = None;
        Ok(())
    }

    /// Drops the alternative at `index`. Returns whether any are left.
    ///
    /// By index rather than by component id: only a split over *pools* has an id per alternative,
    /// and the caller that removes one — `graph_build::remove_duplicated_routes` — already collects
    /// the ids it is looking for. Indexing keeps this working for a split over chains too.
    pub(crate) fn remove_child(&mut self, index: usize) -> bool {
        if index < self.children.len() {
            self.children.remove(index);
        }
        self.splits.clear();
        self.limit_cache = None;
        !self.children.is_empty()
    }

    /// Whether the alternatives are chains rather than pools — the split a grouped branch puts over
    /// its tails.
    ///
    /// The two kinds are never mixed: `graph_build` builds a leg's pools or a group's tails, never
    /// both at one level.
    pub(crate) fn holds_chains(&self) -> bool {
        self.children
            .iter()
            .any(|child| match child {
                Route::Sequence(_) => true,
                Route::Direct(_) => false,
            })
    }

    /// Position of the alternative trading on `component_id`.
    pub(crate) fn pool_index(&self, component_id: &ComponentId) -> Option<usize> {
        self.children
            .iter()
            .position(|child| match child {
                Route::Direct(pool) => pool.component_id() == component_id,
                Route::Sequence(_) => false,
            })
    }

    /// Drops the alternative trading on `component_id`. Returns whether any are left.
    ///
    /// A level that does not hold it is unchanged, and reports whether it has alternatives at all.
    pub(crate) fn remove_pool(&mut self, component_id: &ComponentId) -> bool {
        match self.pool_index(component_id) {
            Some(index) => self.remove_child(index),
            None => !self.children.is_empty(),
        }
    }

    /// Whether the sell limit is memoised, for tests asserting that [`ParallelRoute::invalidate`]
    /// drops it.
    #[cfg(test)]
    pub(crate) fn has_cached_limit(&self) -> bool {
        self.limit_cache.is_some()
    }

    /// The pools this level divides between, one per alternative and in the same order as
    /// [`ParallelRoute::splits`].
    ///
    /// Only meaningful for a split over pools, which is what a leg of a chain is. A split over
    /// chains reports none, because its alternatives are not pools — use
    /// [`Route::all_pools`](super::Route::all_pools) to reach every leaf below a level.
    pub(crate) fn pools(&self) -> Vec<&Pool> {
        self.children
            .iter()
            .filter_map(|child| match child {
                Route::Direct(pool) => Some(pool),
                Route::Sequence(_) => None,
            })
            .collect()
    }

    /// Solved once the splits are set and every alternative below is (`routes/parallel.py:52-57`).
    pub(crate) fn solved(&self) -> bool {
        !self.splits.is_empty() && self.children.iter().all(Route::solved)
    }

    /// Amount consumed by the last sell.
    #[cfg(test)]
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount produced by the last sell.
    #[cfg(test)]
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// Token this level consumes. Every alternative shares it, so the first one answers.
    pub(crate) fn sell_token(&self) -> &Token {
        self.children[0].sell_token()
    }

    /// Token this level produces.
    pub(crate) fn buy_token(&self) -> &Token {
        self.children[0].buy_token()
    }

    /// [`ParallelRoute::sell_token`] as a shared handle.
    #[cfg(test)]
    pub(crate) fn sell_token_shared(&self) -> Arc<Token> {
        self.children[0].sell_token_shared()
    }

    /// [`ParallelRoute::buy_token`] as a shared handle.
    #[cfg(test)]
    pub(crate) fn buy_token_shared(&self) -> Arc<Token> {
        self.children[0].buy_token_shared()
    }

    // ===================== Composition rules =====================

    /// Whether unsolved estimates apply: no splits, or splits that do not route the whole input
    /// (`routes/parallel.py:78`).
    fn use_estimate(&self) -> bool {
        self.splits.is_empty() || splits_sum(&self.splits) < BigRational::one()
    }

    /// Mean over the alternatives while unsolved, split-weighted sum once solved
    /// (`routes/parallel.py:76-86`).
    fn combine<F>(&self, quantity: F) -> Result<f64, DecompositionError>
    where
        F: Fn(&Route) -> Result<f64, DecompositionError>,
    {
        if self.use_estimate() {
            let mut total = 0.0;
            for child in &self.children {
                total += quantity(child)?;
            }
            return Ok(total / self.children.len() as f64);
        }
        let mut total = 0.0;
        for (child, split) in self.children.iter().zip(&self.splits) {
            total += quantity(child)? * split.to_f64();
        }
        Ok(total)
    }

    /// [`ParallelRoute::combine`] for quantities that cannot fail (`routes/parallel.py:160-170`).
    fn combine_infallible<F>(&self, quantity: F) -> f64
    where
        F: Fn(&Route) -> f64,
    {
        if self.use_estimate() {
            let total: f64 = self
                .children
                .iter()
                .map(&quantity)
                .sum();
            return total / self.children.len() as f64;
        }
        let mut total = 0.0;
        for (child, split) in self.children.iter().zip(&self.splits) {
            total += quantity(child) * split.to_f64();
        }
        total
    }

    /// Spot price before fees.
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        self.combine(Route::route_price)
    }

    /// Price net of fees at the pre-trade state (`routes/parallel.py:108-118`).
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        self.combine(Route::marginal_price)
    }

    /// Marginal price at the post-trade states (`routes/parallel.py:120-134`).
    ///
    /// `None` while unsolved, or when any alternative carrying a non-zero split has none. Unlike
    /// the other price rules this one does *not* fall back to an estimate when the splits do
    /// not sum to one.
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        if self.splits.is_empty() {
            return None;
        }
        let mut total = 0.0;
        for (child, split) in self.children.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            total += child.new_marginal_price()? * split.to_f64();
        }
        Some(total)
    }

    /// Fee as a fraction of the input (`routes/parallel.py:160-170`).
    pub(crate) fn fee(&self) -> f64 {
        self.combine_infallible(Route::fee)
    }

    /// Gas of every pool below this level, whatever the splits (`routes/parallel.py:172-174`).
    pub(crate) fn gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for child in &self.children {
            total += child.gas();
        }
        total
    }

    /// Gas of only the alternatives the splits activate (`routes/parallel.py:281-286`).
    ///
    /// Differs from [`ParallelRoute::gas`] whenever an alternative holds gas from an earlier sell
    /// but ended on a zero split — which is exactly what a split search leaves behind. An
    /// unsolved level activates nothing and reports zero.
    pub(crate) fn minimum_gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for (child, split) in self.children.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            total += child.minimum_gas();
        }
        total
    }

    /// Liquidity depth proxy (`routes/parallel.py:148-151`).
    ///
    /// Deliberately pessimistic while unsolved: the maximum over the alternatives rather than their
    /// sum, because parallel legs usually hold more liquidity than their biggest member and an
    /// optimistic estimate would over-fill the level.
    pub(crate) fn inertia(&self) -> f64 {
        if self.splits.is_empty() {
            return self
                .children
                .iter()
                .map(Route::inertia)
                .fold(f64::NEG_INFINITY, f64::max);
        }
        let mut total = 0.0;
        for (child, split) in self.children.iter().zip(&self.splits) {
            total += child.inertia() * split.to_f64();
        }
        total
    }

    /// Ranking score: maximum over the alternatives while unsolved, split-weighted once solved
    /// (`routes/parallel.py:136-146`).
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        if self.splits.is_empty() {
            let mut best = f64::NEG_INFINITY;
            for child in &self.children {
                best = best.max(child.weight()?);
            }
            return Ok(best);
        }
        let mut total = 0.0;
        for (child, split) in self.children.iter().zip(&self.splits) {
            total += child.weight()? * split.to_f64();
        }
        Ok(total)
    }

    /// Price the last sell achieved, in human units. Gas is not accounted for.
    #[cfg(test)]
    pub(crate) fn executed_price(&self) -> f64 {
        executed_price(&self.sell_amount, self.sell_token(), &self.buy_amount, self.buy_token())
    }

    // ===================== Selling =====================

    /// Routes `amount * split` to each alternative and sums what comes back
    /// (`routes/parallel.py:176-205`).
    ///
    /// # Errors
    ///
    /// [`DecompositionError::Unsolved`] when the splits were never set,
    /// [`DecompositionError::SellAmountLimit`] when `amount` exceeds this level's limit, and
    /// whatever the alternatives raise.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        if !self.solved() {
            return Err(DecompositionError::Unsolved {
                token_in: self.sell_token().address.clone(),
                token_out: self.buy_token().address.clone(),
            });
        }

        let (limit, pools) = self.sell_amount_limit()?;
        if amount > &limit {
            return Err(DecompositionError::SellAmountLimit {
                limit,
                token: self.sell_token().address.clone(),
                pools,
            });
        }

        let mut total_bought = BigUint::zero();
        let mut total_gas = BigUint::zero();
        for index in 0..self.children.len() {
            let child_amount = self.splits[index].apply(amount);
            let (bought, gas) = self.children[index].sell(&child_amount)?;
            total_bought += bought;
            total_gas += gas;
        }

        self.sell_amount = amount.clone();
        self.buy_amount = total_bought.clone();
        Ok((total_bought, total_gas))
    }

    /// Largest amount this level can absorb: the sum over its alternatives
    /// (`routes/parallel.py:216-222`). Cached until [`ParallelRoute::invalidate`].
    pub(crate) fn sell_amount_limit(
        &mut self,
    ) -> Result<(BigUint, Vec<ComponentId>), DecompositionError> {
        if let Some(cached) = self.limit_cache.as_ref() {
            return Ok(cached.clone());
        }
        let mut total = BigUint::zero();
        let mut pools = Vec::with_capacity(self.children.len());
        for child in &mut self.children {
            let (limit, components) = child.sell_amount_limit()?;
            total += limit;
            pools.extend(components);
        }
        let limit = (total, pools);
        self.limit_cache = Some(limit.clone());
        Ok(limit)
    }

    /// Drops this level's cached limit and every cache below it
    /// (`routes/interface.py:286-290`, `:321-327`).
    pub(crate) fn invalidate(&mut self) {
        self.limit_cache = None;
        for child in &mut self.children {
            child.invalidate();
        }
    }
}
