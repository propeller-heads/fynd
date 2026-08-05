//! One leg of a route: the parallel pools between a token pair.

use num_bigint::BigUint;
use num_rational::BigRational;
use num_traits::{One, Zero};
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::components::*;

// ===================== Hop =====================

/// One leg of a [`SequentialRoute`]: parallel pools between the same token pair.
///
/// Equivalent of the `ParallelRoute` of `SimpleRoute`s defibot places at each leg
/// (`routes/parallel.py`). Unsolved (empty `splits`) attributes are estimates over all pools;
/// once solved they become split-weighted.
pub(crate) struct Hop {
    token_in: Token,
    token_out: Token,
    pools: Vec<PoolRef>,
    splits: Vec<Fraction>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    limit_cache: Option<(BigUint, Vec<ComponentId>)>,
}

impl Hop {
    /// Builds an unsolved hop over `pools`.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when `pools` is empty — every composition rule
    /// here averages or maximises over the pools and has no meaning for none of them.
    pub(crate) fn new(
        token_in: Token,
        token_out: Token,
        pools: Vec<PoolRef>,
    ) -> Result<Self, DecompositionError> {
        if pools.is_empty() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!("hop {} -> {} has no pools", token_in.address, token_out.address),
            });
        }
        Ok(Self {
            token_in,
            token_out,
            pools,
            splits: Vec::new(),
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
        })
    }

    /// Input token of this leg.
    pub(crate) fn token_in(&self) -> &Token {
        &self.token_in
    }

    /// Output token of this leg.
    pub(crate) fn token_out(&self) -> &Token {
        &self.token_out
    }

    /// Pools carrying this leg.
    pub(crate) fn pools(&self) -> &[PoolRef] {
        &self.pools
    }

    /// Mutable pools, for optimizers selling trial amounts on them.
    pub(crate) fn pools_mut(&mut self) -> &mut [PoolRef] {
        &mut self.pools
    }

    /// Whether the hop's sell limit is memoised. Test-only; see [`PoolRef::has_cached_limit`].
    #[cfg(test)]
    pub(crate) fn has_cached_limit(&self) -> bool {
        self.limit_cache.is_some()
    }

    /// Share of the hop's input routed to each pool, empty while unsolved.
    pub(crate) fn splits(&self) -> &[Fraction] {
        &self.splits
    }

    /// Assigns one split per pool, or an empty vector to mark the hop unsolved again.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when a non-empty split vector does not match the
    /// pool count.
    pub(crate) fn set_splits(&mut self, splits: Vec<Fraction>) -> Result<(), DecompositionError> {
        if !splits.is_empty() && splits.len() != self.pools.len() {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "hop {} -> {} has {} pools but got {} splits",
                    self.token_in.address,
                    self.token_out.address,
                    self.pools.len(),
                    splits.len()
                ),
            });
        }
        self.splits = splits;
        Ok(())
    }

    /// Drops the pool with this component id, leaving the hop unsolved.
    ///
    /// Returns whether the hop still holds a pool: a hop emptied by the removal is invalid
    /// ([`Hop::new`] refuses it), so the caller must drop whatever contains it — which is exactly
    /// the branch `_remove_duplicated_routes` takes when the parallel route would be emptied
    /// (`order_solver.py:762-772`).
    ///
    /// The splits are cleared because they were sized for a different pool set; leaving them would
    /// silently reroute the removed pool's share onto its neighbours.
    pub(crate) fn remove_pool(&mut self, component_id: &ComponentId) -> bool {
        self.pools
            .retain(|pool| pool.component_id() != component_id);
        self.splits.clear();
        self.limit_cache = None;
        !self.pools.is_empty()
    }

    /// A hop is solved once its splits are set (`routes/parallel.py:52-57`).
    pub(crate) fn solved(&self) -> bool {
        !self.splits.is_empty()
    }

    /// Amount of [`Hop::token_in`] the last sell consumed.
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount of [`Hop::token_out`] the last sell produced.
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// Whether unsolved estimates apply: no splits, or splits that do not route the whole input
    /// (`routes/parallel.py:78`).
    fn use_estimate(&self) -> bool {
        !self.solved() || splits_sum(&self.splits) < BigRational::one()
    }

    /// Spot price of the hop: arithmetic mean over pools while unsolved, split-weighted sum once
    /// solved (`routes/parallel.py:76-86`).
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        self.combine(|pool, token_in, token_out| pool.route_price(token_in, token_out))
    }

    /// Hop price net of fees (`routes/parallel.py:108-118`).
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        self.combine(|pool, token_in, token_out| pool.marginal_price(token_in, token_out))
    }

    /// Marginal price at the post-trade states (`routes/parallel.py:120-134`).
    ///
    /// `None` when the hop is unsolved, or when any pool carrying a non-zero split was not sold
    /// on. Note that unlike the other price rules this one does *not* fall back to an estimate
    /// when the splits do not sum to one.
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        if !self.solved() {
            return None;
        }
        let mut total = 0.0;
        for (pool, split) in self.pools.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            let Some(price) = pool.new_marginal_price(&self.token_in, &self.token_out) else {
                return None;
            };
            total += price * split.to_f64();
        }
        Some(total)
    }

    /// Hop fee: arithmetic mean over pools while unsolved, split-weighted once solved
    /// (`routes/parallel.py:160-170`).
    pub(crate) fn fee(&self) -> f64 {
        self.combine_infallible(PoolRef::fee)
    }

    /// Gas of every pool in the hop (`routes/parallel.py:172-174`).
    pub(crate) fn gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for pool in &self.pools {
            total += pool.gas();
        }
        total
    }

    /// Gas of only the pools the hop's splits activate (`routes/parallel.py:281-286`).
    ///
    /// Differs from [`Hop::gas`] whenever a pool holds gas from an earlier sell but ended up with a
    /// zero split — which is exactly what a split search leaves behind. An unsolved hop activates
    /// nothing and reports zero.
    pub(crate) fn minimum_gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for (pool, split) in self.pools.iter().zip(&self.splits) {
            if split.is_zero() {
                continue;
            }
            total += pool.gas();
        }
        total
    }

    /// Liquidity depth proxy (`routes/parallel.py:148-151`).
    ///
    /// Deliberately pessimistic while unsolved: the maximum over the pools rather than their sum,
    /// because parallel legs usually hold more liquidity than their biggest member and an
    /// optimistic estimate would over-fill the hop.
    pub(crate) fn inertia(&self) -> f64 {
        if !self.solved() {
            return self
                .pools
                .iter()
                .map(|pool| pool.inertia(&self.token_in))
                .fold(f64::NEG_INFINITY, f64::max);
        }
        let mut total = 0.0;
        for (pool, split) in self.pools.iter().zip(&self.splits) {
            total += pool.inertia(&self.token_in) * split.to_f64();
        }
        total
    }

    /// Ranking score: maximum over pools while unsolved, split-weighted once solved
    /// (`routes/parallel.py:136-146`).
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        if !self.solved() {
            let mut best = f64::NEG_INFINITY;
            for pool in &self.pools {
                best = best.max(pool.weight(&self.token_in, &self.token_out)?);
            }
            return Ok(best);
        }
        let mut total = 0.0;
        for (pool, split) in self.pools.iter().zip(&self.splits) {
            total += pool.weight(&self.token_in, &self.token_out)? * split.to_f64();
        }
        Ok(total)
    }

    /// Price actually achieved by the last sell, in human units. Gas is not accounted for.
    pub(crate) fn executed_price(&self) -> f64 {
        executed_price(&self.sell_amount, &self.token_in, &self.buy_amount, &self.token_out)
    }

    /// Sells `amount` of [`Hop::token_in`], routing `amount * split` to each pool and summing the
    /// results (`routes/parallel.py:176-205`).
    ///
    /// Returns the bought amount and the total gas.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::Unsolved`] when the splits were never set,
    /// [`DecompositionError::SellAmountLimit`] when `amount` exceeds the hop's limit, and
    /// whatever the underlying pools raise.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        if !self.solved() {
            return Err(DecompositionError::Unsolved {
                token_in: self.token_in.address.clone(),
                token_out: self.token_out.address.clone(),
            });
        }

        let (limit, pools) = self.sell_amount_limit()?;
        if amount > &limit {
            return Err(DecompositionError::SellAmountLimit {
                limit,
                token: self.token_in.address.clone(),
                pools,
            });
        }

        let mut total_bought = BigUint::zero();
        let mut total_gas = BigUint::zero();
        for index in 0..self.pools.len() {
            let pool_amount = self.splits[index].apply(amount);
            let (token_in, token_out) = (self.token_in.clone(), self.token_out.clone());
            let (bought, gas) = self.pools[index].sell(&pool_amount, &token_in, &token_out)?;
            total_bought += bought;
            total_gas += gas;
        }

        self.sell_amount = amount.clone();
        self.buy_amount = total_bought.clone();
        Ok((total_bought, total_gas))
    }

    /// Largest amount of [`Hop::token_in`] the hop can absorb: the sum over its parallel pools
    /// (`routes/parallel.py:216-222`). Cached until [`Hop::invalidate`].
    pub(crate) fn sell_amount_limit(
        &mut self,
    ) -> Result<(BigUint, Vec<ComponentId>), DecompositionError> {
        if let Some(cached) = self.limit_cache.as_ref() {
            return Ok(cached.clone());
        }
        let mut total = BigUint::zero();
        let mut pools = Vec::with_capacity(self.pools.len());
        let (token_in, token_out) = (self.token_in.clone(), self.token_out.clone());
        for pool in &mut self.pools {
            total += pool.sell_amount_limit(&token_in, &token_out)?;
            pools.push(pool.component_id().clone());
        }
        self.limit_cache = Some((total.clone(), pools.clone()));
        Ok((total, pools))
    }

    /// Drops this hop's cached limit and every pool cache below it
    /// (`routes/interface.py:286-290`, `:321-327`).
    pub(crate) fn invalidate(&mut self) {
        self.limit_cache = None;
        for pool in &mut self.pools {
            pool.invalidate();
        }
    }

    /// Mean over pools while unsolved, split-weighted sum once solved.
    fn combine<F>(&self, quantity: F) -> Result<f64, DecompositionError>
    where
        F: Fn(&PoolRef, &Token, &Token) -> Result<f64, DecompositionError>,
    {
        if self.use_estimate() {
            let mut total = 0.0;
            for pool in &self.pools {
                total += quantity(pool, &self.token_in, &self.token_out)?;
            }
            return Ok(total / self.pools.len() as f64);
        }
        let mut total = 0.0;
        for (pool, split) in self.pools.iter().zip(&self.splits) {
            total += quantity(pool, &self.token_in, &self.token_out)? * split.to_f64();
        }
        Ok(total)
    }

    /// [`Hop::combine`] for quantities that cannot fail.
    fn combine_infallible<F>(&self, quantity: F) -> f64
    where
        F: Fn(&PoolRef) -> f64,
    {
        if self.use_estimate() {
            let total: f64 = self.pools.iter().map(&quantity).sum();
            return total / self.pools.len() as f64;
        }
        let mut total = 0.0;
        for (pool, split) in self.pools.iter().zip(&self.splits) {
            total += quantity(pool) * split.to_f64();
        }
        total
    }
}
