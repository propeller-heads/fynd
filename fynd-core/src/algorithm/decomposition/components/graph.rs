//! The whole solution: parallel branches over one order.

use num_bigint::BigUint;
#[cfg(test)]
use num_rational::BigRational;
#[cfg(test)]
use num_traits::One;
use num_traits::Zero;
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::components::*;

// ===================== SolutionGraph =====================

/// A complete solution: parallel branches over one order, each a [`SequenceRoute`].
///
/// Equivalent of the top-level `Route` in defibot. Every branch shares the same sell and
/// buy token; `outer_splits` says how much of the order each branch carries, and is empty while
/// the graph is unsolved — the same encoding [`Route`] uses for `splits`.
///
/// The outer splits are over **branches, not token paths**. Two token paths that leave the sell
/// token through the same pool belong to one branch, so no pool the two share can be allocated
/// twice at this level (`order_solver.py:517-554`).
pub(crate) struct DecompositionGraph {
    pub(crate) sequences: Vec<SequenceRoute>,
    splits: Vec<Fraction>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    limit_cache: Option<(BigUint, Vec<ComponentId>)>,
}

impl DecompositionGraph {
    /// Builds a solution graph over parallel branches.
    ///
    /// Pass an empty `outer_splits` to build the graph unsolved — candidate discovery assembles
    /// the top-level parallel route before any splits exist and ranks it on
    /// [`DecompositionGraph::weight`], which must take the unsolved branch.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when there are no branches, when a non-empty split
    /// vector does not match the branch count, or when the branches do not share both endpoints
    /// (`routes/parallel.py:289-292`).
    pub(crate) fn new(
        branches: Vec<SequenceRoute>,
        outer_splits: Vec<Fraction>,
    ) -> Result<Self, DecompositionError> {
        if branches.is_empty() {
            return Err(DecompositionError::InvalidStructure {
                reason: "solution graph has no branches".to_string(),
            });
        }
        if !outer_splits.is_empty() && branches.len() != outer_splits.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "solution graph has {} branches but {} splits",
                    branches.len(),
                    outer_splits.len()
                ),
            });
        }
        let (sell_token, buy_token) =
            (branches[0].sell_token().address.clone(), branches[0].buy_token().address.clone());
        for branch in &branches {
            if branch.sell_token().address != sell_token || branch.buy_token().address != buy_token
            {
                return Err(DecompositionError::InvalidStructure {
                    reason: format!(
                        "branch {} -> {} does not share endpoints {sell_token} -> {buy_token}",
                        branch.sell_token().address,
                        branch.buy_token().address
                    ),
                });
            }
        }
        Ok(Self {
            sequences: branches,
            splits: outer_splits,
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
        })
    }

    /// Share of the order each branch carries.
    pub(crate) fn outer_splits(&self) -> &[Fraction] {
        &self.splits
    }

    /// Assigns one split per branch, or an empty vector to mark the graph unsolved again.
    ///
    /// Clearing the splits is how defibot forces a re-solve — `order_solver.py:717`
    /// (`reset_splits`) and `order_solver.py:893` (`remove_loops`) both assign `splits = None`.
    ///
    /// # Errors
    ///
    /// Supplies the derived mid-prices to every branch of the graph.
    ///
    /// See [`Route::set_prices`].
    pub(crate) fn set_prices(&mut self, prices: Arc<TokenGasPrices>) {
        for branch in &mut self.sequences {
            branch.set_prices(Arc::clone(&prices));
        }
        self.limit_cache = None;
    }

    /// [`DecompositionError::InvalidStructure`] when a non-empty split vector does not match the
    /// branch count.
    pub(crate) fn set_outer_splits(
        &mut self,
        outer_splits: Vec<Fraction>,
    ) -> Result<(), DecompositionError> {
        if !outer_splits.is_empty() && outer_splits.len() != self.sequences.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "solution graph has {} branches but got {} splits",
                    self.sequences.len(),
                    outer_splits.len()
                ),
            });
        }
        self.splits = outer_splits;
        Ok(())
    }

    /// Token sold by the order.
    pub(crate) fn sell_token(&self) -> &Token {
        self.sequences[0].sell_token()
    }

    /// Token bought by the order.
    pub(crate) fn buy_token(&self) -> &Token {
        self.sequences[0].buy_token()
    }

    /// Amount of [`DecompositionGraph::sell_token`] the last sell consumed.
    #[cfg(test)]
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount of [`DecompositionGraph::buy_token`] the last sell produced.
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// A graph is solved once its outer splits are set and every branch is solved
    /// (`routes/parallel.py:52-57`).
    pub(crate) fn solved(&self) -> bool {
        !self.splits.is_empty() &&
            self.sequences
                .iter()
                .all(SequenceRoute::solved)
    }

    /// Whether unsolved estimates apply (`routes/parallel.py:78`).
    #[cfg(test)]
    fn use_estimate(&self) -> bool {
        !self.solved() || splits_sum(&self.splits) < BigRational::one()
    }

    /// Mean of the branch prices while unsolved, split-weighted sum once solved
    /// (`routes/parallel.py:76-86`).
    ///
    /// Parity-only, like [`DecompositionGraph::inertia`] and [`DecompositionGraph::weight`]. The
    /// solve reaches a graph through [`Sellable`](super::optimizers::Sellable), which asks for
    /// `marginal_price`, `minimum_gas`, `executed_price` and `sell` and nothing else. These three
    /// composition rules are ported and tested against defibot's numbers so a future optimizer
    /// working at the graph level inherits them correct, but nothing reads them today, so they are
    /// not compiled into the library.
    #[cfg(test)]
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        self.combine(|branch| branch.route_price())
    }

    /// Branch prices net of fees (`routes/parallel.py:108-118`). Parity-only; see
    /// [`DecompositionGraph::route_price`].
    #[cfg(test)]
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        self.combine(|branch| branch.marginal_price())
    }

    /// Marginal price at the post-trade states (`routes/parallel.py:120-134`).
    ///
    /// `None` when the graph is unsolved or when any branch carrying a non-zero split has none.
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        if !self.solved() {
            return None;
        }
        let mut total = 0.0;
        for (branch, split) in self.sequences.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            let Some(price) = branch.new_marginal_price() else {
                return None;
            };
            total += price * split.to_f64();
        }
        Some(total)
    }

    /// Gas of every branch (`routes/parallel.py:172-174`).
    pub(crate) fn gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for branch in &self.sequences {
            total += branch.gas();
        }
        total
    }

    /// Keeps the branches whose entry in `keep` is true, leaving the graph unsolved.
    ///
    /// Loop removal (`order_solver.py:888-893`) drops whole branches and then relies on the caller
    /// re-solving, so the outer splits are cleared here: they were computed for a different branch
    /// set and carrying them over would silently reroute the amounts.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when `keep` does not cover every branch, or when it
    /// would leave no branch at all — defibot assigns the empty list and produces a graph that
    /// raises on the next attribute access (`routes/parallel.py:289-292` never runs again).
    pub(crate) fn retain_branches(&mut self, keep: &[bool]) -> Result<(), DecompositionError> {
        if keep.len() != self.sequences.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "solution graph has {} branches but got {} keep flags",
                    self.sequences.len(),
                    keep.len()
                ),
            });
        }
        if !keep.iter().any(|keep| *keep) {
            return Err(DecompositionError::InvalidStructure {
                reason: "removing every branch would leave an empty solution graph".to_string(),
            });
        }

        let mut kept = Vec::with_capacity(self.sequences.len());
        for (index, branch) in self.sequences.drain(..).enumerate() {
            if keep[index] {
                kept.push(branch);
            }
        }
        self.sequences = kept;
        self.splits.clear();
        self.limit_cache = None;
        Ok(())
    }

    /// Records amounts produced by a sell this graph did not drive itself.
    ///
    /// Coupled-path selling walks the branches one at a time instead of going through
    /// [`DecompositionGraph::sell`], and writes the totals back the same way defibot does
    /// (`utils.py:43-44`).
    pub(crate) fn record_sell(&mut self, sell_amount: BigUint, buy_amount: BigUint) {
        self.sell_amount = sell_amount;
        self.buy_amount = buy_amount;
    }

    /// Liquidity depth proxy: pessimistically the deepest branch while unsolved
    /// (`routes/parallel.py:148-151`). Parity-only; see [`DecompositionGraph::route_price`].
    #[cfg(test)]
    pub(crate) fn inertia(&self) -> f64 {
        if !self.solved() {
            return self
                .sequences
                .iter()
                .map(|branch| branch.inertia())
                .fold(f64::NEG_INFINITY, f64::max);
        }
        let mut total = 0.0;
        for (branch, split) in self.sequences.iter().zip(&self.splits) {
            total += branch.inertia() * split.to_f64();
        }
        total
    }

    /// Ranking score: maximum over branches while unsolved, split-weighted once solved
    /// (`routes/parallel.py:136-146`). Parity-only; see [`DecompositionGraph::route_price`].
    #[cfg(test)]
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        if !self.solved() {
            let mut best = f64::NEG_INFINITY;
            for branch in &self.sequences {
                best = best.max(branch.weight()?);
            }
            return Ok(best);
        }
        let mut total = 0.0;
        for (branch, split) in self.sequences.iter().zip(&self.splits) {
            total += branch.weight()? * split.to_f64();
        }
        Ok(total)
    }

    /// Price actually achieved by the last sell, in human units. Gas is not accounted for.
    pub(crate) fn executed_price(&self) -> f64 {
        executed_price(&self.sell_amount, self.sell_token(), &self.buy_amount, self.buy_token())
    }

    /// Sells `amount`, routing `amount * split` down each branch and summing the results
    /// (`routes/parallel.py:176-205`).
    ///
    /// Returns the bought amount and the total gas.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::Unsolved`] when the outer splits were never set (defibot raises
    /// `AttributeError`, `routes/parallel.py:179-180`),
    /// [`DecompositionError::SellAmountLimit`] when `amount` exceeds the graph's limit, and
    /// whatever the branches raise.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        if self.splits.is_empty() {
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
        for index in 0..self.sequences.len() {
            let branch_amount = self.splits[index].apply(amount);
            let (bought, gas) = self.sequences[index].sell(&branch_amount)?;
            total_bought += bought;
            total_gas += gas;
        }

        self.sell_amount = amount.clone();
        self.buy_amount = total_bought.clone();
        Ok((total_bought, total_gas))
    }

    /// Largest amount the graph can absorb: the sum over its parallel branches
    /// (`routes/parallel.py:216-222`). Cached until [`DecompositionGraph::invalidate`].
    pub(crate) fn sell_amount_limit(
        &mut self,
    ) -> Result<(BigUint, Vec<ComponentId>), DecompositionError> {
        if let Some(cached) = self.limit_cache.as_ref() {
            return Ok(cached.clone());
        }
        let mut total = BigUint::zero();
        let mut pools = Vec::new();
        for branch in &mut self.sequences {
            let (limit, branch_pools) = branch.sell_amount_limit()?;
            debug!(
                branch = %branch.token_path_label(),
                contribution = %limit,
                "graph sell limit: one branch's contribution"
            );
            total += limit;
            pools.extend(branch_pools);
        }
        self.limit_cache = Some((total.clone(), pools.clone()));
        Ok((total, pools))
    }

    /// Mean over branches while unsolved, split-weighted sum once solved.
    #[cfg(test)]
    fn combine<F>(&self, quantity: F) -> Result<f64, DecompositionError>
    where
        F: Fn(&SequenceRoute) -> Result<f64, DecompositionError>,
    {
        if self.use_estimate() {
            let mut total = 0.0;
            for branch in &self.sequences {
                total += quantity(branch)?;
            }
            return Ok(total / self.sequences.len() as f64);
        }
        let mut total = 0.0;
        for (branch, split) in self.sequences.iter().zip(&self.splits) {
            total += quantity(branch)? * split.to_f64();
        }
        Ok(total)
    }
}
