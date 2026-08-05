//! A token path with one hop per leg.

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::Zero;
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::components::*;

// ===================== SequentialRoute =====================

/// One parallel branch of a [`SolutionGraph`]: a token path with one [`Hop`] per leg.
///
/// Equivalent of defibot's `SequentialRoute` (`routes/sequential.py`). Composed attributes are
/// products (prices), sums (gas) or minima (inertia) over the hops.
pub(crate) struct SequentialRoute {
    tokens: Vec<Token>,
    hops: Vec<Hop>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    limit_cache: Option<(BigUint, Vec<ComponentId>)>,
    /// Derived mid-prices for denominating limits, or `None` to fall back to spot prices.
    ///
    /// Context rather than state, carried here because the limit is computed inside `sell`, which
    /// `Sellable` gives no place to pass it. See [`SequentialRoute::set_prices`].
    prices: Option<Arc<TokenGasPrices>>,
}

impl SequentialRoute {
    /// Builds a route over a token path.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when the route has no hops, when `tokens` is not
    /// one longer than `hops`, or when a hop does not connect the tokens at its position
    /// (`routes/sequential.py:233-238`).
    pub(crate) fn new(tokens: Vec<Token>, hops: Vec<Hop>) -> Result<Self, DecompositionError> {
        if hops.is_empty() {
            return Err(DecompositionError::InvalidStructure {
                reason: "sequential route has no hops".to_string(),
            });
        }
        if tokens.len() != hops.len() + 1 {
            return Err(DecompositionError::InvalidStructure {
                reason: format!(
                    "sequential route has {} hops but {} tokens; expected {}",
                    hops.len(),
                    tokens.len(),
                    hops.len() + 1
                ),
            });
        }
        for (index, hop) in hops.iter().enumerate() {
            if hop.token_in().address != tokens[index].address ||
                hop.token_out().address != tokens[index + 1].address
            {
                return Err(DecompositionError::InvalidStructure {
                    reason: format!(
                        "hop {index} connects {} -> {} but the path expects {} -> {}",
                        hop.token_in().address,
                        hop.token_out().address,
                        tokens[index].address,
                        tokens[index + 1].address
                    ),
                });
            }
        }
        Ok(Self {
            prices: None,
            tokens,
            hops,
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
        })
    }

    /// Legs of this branch.
    pub(crate) fn hops(&self) -> &[Hop] {
        &self.hops
    }

    /// Mutable legs, for solvers assigning splits.
    pub(crate) fn hops_mut(&mut self) -> &mut [Hop] {
        &mut self.hops
    }

    /// The leg at `index`. Assertion sugar, matching [`Branch::hop`].
    #[cfg(test)]
    pub(crate) fn hop(&self, index: usize) -> &Hop {
        &self.hops[index]
    }

    /// The leg at `index`, mutably. Assertion sugar, matching [`Branch::hop_mut`].
    #[cfg(test)]
    pub(crate) fn hop_mut(&mut self, index: usize) -> &mut Hop {
        &mut self.hops[index]
    }

    /// Supplies the derived mid-prices used to denominate limits, replacing the spot-price chain.
    ///
    /// Called once after construction rather than passed to the constructor, so the many existing
    /// call sites and fixtures need no change. A route left without prices keeps the old
    /// behaviour.
    pub(crate) fn set_prices(&mut self, prices: Arc<TokenGasPrices>) {
        self.prices = Some(prices);
        self.limit_cache = None;
    }

    /// Consumes the route into its token path and its legs.
    ///
    /// Reference-route assembly stitches two independently built one-hop routes into a two-hop one
    /// (`order_solver.py:366-368`), which needs the hops themselves rather than a view of them.
    pub(crate) fn into_parts(self) -> (Vec<Token>, Vec<Hop>) {
        (self.tokens, self.hops)
    }

    /// Token sold at the start of the path.
    pub(crate) fn sell_token(&self) -> &Token {
        &self.tokens[0]
    }

    /// Token bought at the end of the path.
    pub(crate) fn buy_token(&self) -> &Token {
        &self.tokens[self.tokens.len() - 1]
    }

    /// Amount of [`SequentialRoute::sell_token`] the last sell consumed.
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount of [`SequentialRoute::buy_token`] the last sell produced.
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// A route is solved once every hop is (`routes/sequential.py:163-165`).
    pub(crate) fn solved(&self) -> bool {
        self.hops.iter().all(Hop::solved)
    }

    /// Product of the hop prices (`routes/sequential.py:61-65`).
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        let mut price = 1.0;
        for hop in &self.hops {
            price *= hop.route_price()?;
        }
        Ok(price)
    }

    /// Product of the hop marginal prices (`routes/sequential.py:76-81`).
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        let mut price = 1.0;
        for hop in &self.hops {
            price *= hop.marginal_price()?;
        }
        Ok(price)
    }

    /// Product of the hop post-trade marginal prices, `None` if any hop has none
    /// (`routes/sequential.py:84-90`).
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        let mut price = 1.0;
        for hop in &self.hops {
            let Some(hop_price) = hop.new_marginal_price() else {
                return None;
            };
            price *= hop_price;
        }
        Some(price)
    }

    /// Series composition of the hop fees: `1 - Π(1 - fee_i)` (`routes/sequential.py:98`).
    pub(crate) fn fee(&self) -> f64 {
        let mut remaining = 1.0;
        for hop in &self.hops {
            remaining *= 1.0 - hop.fee();
        }
        1.0 - remaining
    }

    /// Gas of every hop (`routes/sequential.py:101-102`).
    pub(crate) fn gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for hop in &self.hops {
            total += hop.gas();
        }
        total
    }

    /// Gas of only the pools this route's hops activate (`routes/sequential.py:93-94`).
    ///
    /// See [`Hop::minimum_gas`] for how it differs from [`SequentialRoute::gas`].
    pub(crate) fn minimum_gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for hop in &self.hops {
            total += hop.minimum_gas();
        }
        total
    }

    /// Depth of the shallowest hop — a chain absorbs no more than its narrowest link
    /// (`routes/sequential.py:105-106`).
    pub(crate) fn inertia(&self) -> f64 {
        self.hops
            .iter()
            .map(Hop::inertia)
            .fold(f64::INFINITY, f64::min)
    }

    /// Ranking score: `inertia * (1 - fee) * route_price` (`routes/sequential.py:109-111`).
    ///
    /// A one-hop route delegates to its hop instead. defibot never wraps a single-hop token
    /// sequence in a `SequentialRoute` — it appends the bare `ParallelRoute` built by
    /// `_create_one_hop_route` (`order_solver.py:450-456`), whose unsolved weight is the maximum
    /// over its pools (`routes/parallel.py:136-146`). The fixed structure has no such shape, so
    /// without this the composed formula would apply the *mean* pool price to the *maximum* pool
    /// inertia and can score above every individual pool. That inflates single-hop branches
    /// against multi-hop ones, and branch order decides which survive the candidate cap.
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        if let [hop] = self.hops.as_slice() {
            return hop.weight();
        }
        Ok(self.route_price()? * (1.0 - self.fee()) * self.inertia())
    }

    /// Price actually achieved by the last sell, in human units. Gas is not accounted for.
    pub(crate) fn executed_price(&self) -> f64 {
        executed_price(&self.sell_amount, self.sell_token(), &self.buy_amount, self.buy_token())
    }

    /// Sells `amount` along the path, feeding hop `i`'s output into hop `i + 1`
    /// (`routes/sequential.py:113-154`).
    ///
    /// Returns the bought amount and the total gas.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::SellAmountLimit`] with the limit expressed in
    /// [`SequentialRoute::sell_token`] units — a limit hit at an intermediate token is cast back
    /// so the caller can retry with `limit - 1` in the units it sells.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        let (limit, pools) = self.sell_amount_limit()?;
        if amount > &limit {
            return Err(DecompositionError::SellAmountLimit {
                limit,
                token: self.sell_token().address.clone(),
                pools,
            });
        }

        let mut hop_amount = amount.clone();
        let mut total_gas = BigUint::zero();
        for index in 0..self.hops.len() {
            match self.hops[index].sell(&hop_amount) {
                Ok((bought, gas)) => {
                    hop_amount = bought;
                    total_gas += gas;
                }
                Err(DecompositionError::SellAmountLimit { limit, pools, .. }) => {
                    return Err(DecompositionError::SellAmountLimit {
                        limit: self.cast_to_sell_token(index, &limit)?,
                        token: self.sell_token().address.clone(),
                        pools,
                    });
                }
                Err(other) => return Err(other),
            }
        }

        self.sell_amount = amount.clone();
        self.buy_amount = hop_amount.clone();
        Ok((hop_amount, total_gas))
    }

    /// Largest amount of [`SequentialRoute::sell_token`] the path can absorb: the minimum over
    /// the hop limits, each cast back to sell-token units (`routes/sequential.py:176-185`).
    ///
    /// A hop with a limit of zero short-circuits the whole route to zero. Cached until
    /// [`SequentialRoute::invalidate`].
    pub(crate) fn sell_amount_limit(
        &mut self,
    ) -> Result<(BigUint, Vec<ComponentId>), DecompositionError> {
        if let Some(cached) = self.limit_cache.as_ref() {
            return Ok(cached.clone());
        }

        let mut hop_limits = Vec::with_capacity(self.hops.len());
        for hop in &mut self.hops {
            hop_limits.push(hop.sell_amount_limit()?);
        }

        let mut best: Option<(BigUint, Vec<ComponentId>)> = None;
        for (index, (limit, pools)) in hop_limits.into_iter().enumerate() {
            if index > 0 && limit.is_zero() {
                let zero = (BigUint::zero(), pools);
                self.limit_cache = Some(zero.clone());
                return Ok(zero);
            }
            let cast = self.cast_to_sell_token(index, &limit)?;
            debug!(
                hop = index,
                token_in = %self.hops[index].token_in().symbol,
                token_out = %self.hops[index].token_out().symbol,
                pools = self.hops[index].pools().len(),
                raw_limit = %limit,
                cast_to_sell_token = %cast,
                "route sell limit: one hop's contribution"
            );
            if best
                .as_ref()
                .is_none_or(|(current, _)| &cast < current)
            {
                best = Some((cast, pools));
            }
        }

        // `best` is always populated: the constructor rejects routes without hops.
        let limit = best.unwrap_or_else(|| (BigUint::zero(), Vec::new()));
        self.limit_cache = Some(limit.clone());
        Ok(limit)
    }

    /// Converts an on-chain amount denominated in `tokens[hop_index]` into sell-token units by
    /// multiplying the reciprocals of the preceding hops' spot prices
    /// (`routes/sequential.py:187-197`).
    ///
    /// This is a linear approximation: it uses spot prices and therefore ignores the price impact
    /// the preceding hops would suffer while actually pushing that amount through. defibot does
    /// the same and the ported optimizers depend on the approximation being cheap.
    pub(crate) fn cast_to_sell_token(
        &self,
        hop_index: usize,
        amount: &BigUint,
    ) -> Result<BigUint, DecompositionError> {
        if hop_index == 0 {
            return Ok(amount.clone());
        }
        // One conversion through the numeraire, rather than a product over every preceding hop's
        // spot price, which compounds each hop's error.
        if let Some(prices) = self.prices.as_ref() {
            if let Some(converted) = convert_through_numeraire(
                prices,
                amount,
                self.hops[hop_index].token_in(),
                self.sell_token(),
            ) {
                return Ok(converted);
            }
        }
        let mut conversion = 1.0;
        for hop in &self.hops[..hop_index] {
            let price = hop.route_price()?;
            if price == 0.0 {
                return Ok(BigUint::zero());
            }
            conversion /= price;
        }

        let Some(conversion) = BigRational::from_float(conversion) else {
            return Err(DecompositionError::InvalidStructure {
                reason: format!("hop prices produced a non-finite conversion factor: {conversion}"),
            });
        };
        let scaled = BigRational::from(BigInt::from(amount.clone())) *
            conversion *
            decimal_scale(self.tokens[0].decimals, self.tokens[hop_index].decimals);
        (scaled.numer() / scaled.denom())
            .to_biguint()
            .ok_or_else(|| DecompositionError::InvalidStructure {
                reason: "cast to sell token produced a negative amount".to_string(),
            })
    }

    /// Drops this route's cached limit and every cache below it.
    pub(crate) fn invalidate(&mut self) {
        self.limit_cache = None;
        for hop in &mut self.hops {
            hop.invalidate();
        }
    }
}
