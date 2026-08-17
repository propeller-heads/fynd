//! Hops in series, and the rules that compose them.

use std::sync::Arc;

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::Zero;
use tycho_simulation::tycho_core::models::token::Token;

use crate::{algorithm::decomposition::components::*, derived::types::TokenGasPrices};

// ===================== SequenceRoute =====================

/// Product over the hops (`routes/sequential.py:61-65`, `:76-81`).
fn hops_product<F>(hops: &[ParallelRoute], quantity: F) -> Result<f64, DecompositionError>
where
    F: Fn(&ParallelRoute) -> Result<f64, DecompositionError>,
{
    let mut price = 1.0;
    for hop in hops {
        price *= quantity(hop)?;
    }
    Ok(price)
}

/// Series composition of the hop fees: `1 - Π(1 - fee_i)` (`routes/sequential.py:98`).
fn hops_fee(hops: &[ParallelRoute]) -> f64 {
    let mut remaining = 1.0;
    for hop in hops {
        remaining *= 1.0 - hop.fee();
    }
    1.0 - remaining
}

/// Depth of the shallowest hop — a chain absorbs no more than its narrowest link
/// (`routes/sequential.py:105-106`).
fn hops_inertia(hops: &[ParallelRoute]) -> f64 {
    hops.iter()
        .map(ParallelRoute::inertia)
        .fold(f64::INFINITY, f64::min)
}

/// One hop rendered for [`SequenceRoute::token_path_label`]: the token it ends at, or the paths
/// through the chains it holds.
fn hop_label(hop: &ParallelRoute) -> String {
    let mut chains = Vec::new();
    for child in hop.inner() {
        if let SplitKind::Sequence(chain) = child {
            chains.push(chain.continuation_label());
        }
    }
    match chains.len() {
        0 => hop.buy_token().symbol.clone(),
        1 => chains.remove(0),
        _ => format!("[{}]", chains.join(" | ")),
    }
}

/// Ranking score of a chain of hops: `inertia * (1 - fee) * route_price`
/// (`routes/sequential.py:109-111`).
///
/// A free function because candidate discovery ranks token sequences *before* it knows which of
/// them become branch tails, and so before it has a [`SequenceRoute`] to ask.
///
/// A chain of one hop delegates to that hop instead. defibot never wraps a single-hop token
/// sequence in a `SequentialRoute` — it appends the bare `ParallelRoute` built by
/// `_create_one_hop_route` (`order_solver.py:450-456`), whose unsolved weight is the maximum over
/// its pools (`routes/parallel.py:136-146`). Without this the composed formula would apply the
/// *mean* pool price to the *maximum* pool inertia and can score above every individual pool. That
/// inflates single-hop candidates against multi-hop ones, and their order decides which survive the
/// cap.
///
/// # Errors
///
/// Whatever pricing a hop raises.
pub(crate) fn sequence_weight(hops: &[ParallelRoute]) -> Result<f64, DecompositionError> {
    if let [hop] = hops {
        return hop.weight();
    }
    Ok(hops_product(hops, ParallelRoute::route_price)? *
        (1.0 - hops_fee(hops)) *
        hops_inertia(hops))
}

/// Hops in series: each one's output is the next one's input.
///
/// Equivalent of defibot's `SequentialRoute` (`routes/sequential.py`). Composed attributes are
/// products (prices), sums (gas) or minima (inertia) over the hops.
///
/// There is no token vector: the tokens come from the pools, so a chain and its token path cannot
/// disagree — which is what the old constructor had to check.
pub(crate) struct SequenceRoute {
    hops: Vec<ParallelRoute>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    /// Tightest hop limit cast to sell-token units, memoised until [`SequenceRoute::invalidate`].
    limit_cache: Option<(BigUint, Vec<ComponentId>)>,
    /// Derived mid-prices for denominating limits, or `None` to fall back to chained spot prices.
    ///
    /// Context rather than state, carried here because the cast happens inside
    /// [`SequenceRoute::sell`], which takes an amount and nothing else. It belongs in a solve
    /// context; it moves when the swap caches do — see the pure-quoting item in `TODO.md`.
    prices: Option<Arc<TokenGasPrices>>,
}

impl SequenceRoute {
    /// Builds a chain over `hops`.
    ///
    /// # Errors
    ///
    /// [`DecompositionError::InvalidStructure`] when `hops` is empty, or when a hop does not start
    /// where the one before it ended (`routes/sequential.py:233-238`).
    pub(crate) fn new(hops: Vec<ParallelRoute>) -> Result<Self, DecompositionError> {
        if hops.is_empty() {
            return Err(DecompositionError::InvalidStructure {
                reason: "a sequence needs at least one hop".to_string(),
            });
        }
        for pair in hops.windows(2) {
            if pair[0].buy_token().address != pair[1].sell_token().address {
                return Err(DecompositionError::InvalidStructure {
                    reason: format!(
                        "hop ending in {} is followed by one starting at {}",
                        pair[0].buy_token().address,
                        pair[1].sell_token().address,
                    ),
                });
            }
        }
        Ok(Self {
            hops,
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            limit_cache: None,
            prices: None,
        })
    }

    /// The hops, in the order they trade.
    pub(crate) fn hops(&self) -> &[ParallelRoute] {
        &self.hops
    }

    /// The hops, for the solve to set splits on.
    pub(crate) fn hops_mut(&mut self) -> &mut [ParallelRoute] {
        &mut self.hops
    }

    /// Adds every hop of this chain and of every chain below it, in flow order.
    ///
    /// A hop whose alternatives are chains is added *before* their hops, so this is the flow order
    /// of a grouped branch: the shared hop first when it leads, last when it trails.
    ///
    /// **The order is load-bearing for [`remove_loops`](super::super::solve::remove_loops)**, which
    /// registers token directions as it walks and lets the first claimer win. A hop holding chains
    /// claims nothing itself — [`ParallelRoute::pools`] keeps only pool alternatives, so such a hop
    /// reports none — and only its position among the others matters.
    #[cfg(test)]
    pub(crate) fn all_hops(&self) -> Vec<&ParallelRoute> {
        let mut found = Vec::new();
        self.collect_hops(&mut found);
        found
    }

    pub(super) fn collect_hops<'a>(&'a self, found: &mut Vec<&'a ParallelRoute>) {
        for hop in &self.hops {
            found.push(hop);
            for child in hop.inner() {
                child.collect_hops(found);
            }
        }
    }

    /// Runs `visit` on every pool below this chain. See [`SplitKind::for_each_pool_mut`].
    pub(crate) fn for_each_pool_mut(&mut self, visit: &mut impl FnMut(&mut Pool)) {
        for hop in &mut self.hops {
            for child in hop.inner_mut() {
                child.for_each_pool_mut(visit);
            }
        }
    }

    /// Marks this chain and everything below it unsolved (`order_solver.py:714-720`).
    ///
    /// A hop's splits divided an amount produced at the old size, and the chain is about to be
    /// re-solved at a different one.
    pub(crate) fn reset_splits(&mut self) {
        for hop in &mut self.hops {
            for child in hop.inner_mut() {
                if let SplitKind::Sequence(chain) = child {
                    chain.reset_splits();
                }
            }
            // The vector is empty, so the arity check cannot fail.
            let _ = hop.set_splits(Vec::new());
        }
    }

    /// The chain's token path rendered from symbols, as `A->C->B`.
    ///
    /// A hop carrying several chains has several paths through it and renders them together, as
    /// `A->C->[D->B | B]` — which is what tells a grouped branch from an ungrouped one in a log
    /// line or an assertion.
    pub(crate) fn token_path_label(&self) -> String {
        let mut label = self.sell_token().symbol.clone();
        for hop in &self.hops {
            label.push_str("->");
            label.push_str(&hop_label(hop));
        }
        label
    }

    /// The path through this chain without its leading sell token, as `D->B`.
    fn continuation_label(&self) -> String {
        self.hops
            .iter()
            .map(hop_label)
            .collect::<Vec<_>>()
            .join("->")
    }

    /// The hop at `index`.
    ///
    /// Assertion sugar: the production code walks the slice, but a test that knows the chain's
    /// shape wants to name one leg of it.
    #[cfg(test)]
    pub(crate) fn hop(&self, index: usize) -> &ParallelRoute {
        &self.hops[index]
    }

    /// The hop at `index`, mutably. See [`SequenceRoute::hop`].
    #[cfg(test)]
    pub(crate) fn hop_mut(&mut self, index: usize) -> &mut ParallelRoute {
        &mut self.hops[index]
    }

    /// Consumes the chain into its hops.
    ///
    /// Only the fixtures need this: they describe a graph as a list of token paths, and a branch is
    /// built from a path's hops rather than from the chain over them.
    #[cfg(test)]
    pub(crate) fn into_hops(self) -> Vec<ParallelRoute> {
        self.hops
    }

    /// Solved once every hop is (`routes/sequential.py:52-57`).
    pub(crate) fn solved(&self) -> bool {
        self.hops
            .iter()
            .all(ParallelRoute::solved)
    }

    /// Amount consumed by the last sell.
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount produced by the last sell.
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// Token the chain consumes: where its first hop starts.
    pub(crate) fn sell_token(&self) -> &Token {
        self.hops[0].sell_token()
    }

    /// Token the chain produces: where its last hop ends.
    pub(crate) fn buy_token(&self) -> &Token {
        self.hops[self.hops.len() - 1].buy_token()
    }

    /// [`SequenceRoute::sell_token`] as a shared handle.
    #[cfg(test)]
    pub(crate) fn sell_token_shared(&self) -> Arc<Token> {
        self.hops[0].sell_token_shared()
    }

    /// [`SequenceRoute::buy_token`] as a shared handle.
    #[cfg(test)]
    pub(crate) fn buy_token_shared(&self) -> Arc<Token> {
        self.hops[self.hops.len() - 1].buy_token_shared()
    }

    /// Every token on the way through, sell token first.
    #[cfg(test)]
    pub(crate) fn token_path(&self) -> Vec<Arc<Token>> {
        let mut path = Vec::with_capacity(self.hops.len() + 1);
        path.push(self.sell_token_shared());
        for hop in &self.hops {
            path.push(hop.buy_token_shared());
        }
        path
    }

    // ===================== Composition rules =====================

    /// Product of the hop prices (`routes/sequential.py:61-65`).
    pub(crate) fn route_price(&self) -> Result<f64, DecompositionError> {
        hops_product(&self.hops, ParallelRoute::route_price)
    }

    /// Product of the hop marginal prices (`routes/sequential.py:76-81`).
    pub(crate) fn marginal_price(&self) -> Result<f64, DecompositionError> {
        hops_product(&self.hops, ParallelRoute::marginal_price)
    }

    /// Product of the hops' post-trade prices, `None` if any hop has none
    /// (`routes/sequential.py:84-90`).
    pub(crate) fn new_marginal_price(&self) -> Option<f64> {
        let mut price = 1.0;
        for hop in &self.hops {
            price *= hop.new_marginal_price()?;
        }
        Some(price)
    }

    /// Series composition of the hop fees: `1 - Π(1 - fee_i)` (`routes/sequential.py:98`).
    pub(crate) fn fee(&self) -> f64 {
        hops_fee(&self.hops)
    }

    /// Gas of every hop (`routes/sequential.py:101-102`).
    pub(crate) fn gas(&self) -> BigUint {
        let mut total = BigUint::zero();
        for hop in &self.hops {
            total += hop.gas();
        }
        total
    }

    /// Gas of only the pools this chain's hops activate (`routes/sequential.py:93-94`).
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
        hops_inertia(&self.hops)
    }

    /// Ranking score. See [`sequence_weight`].
    pub(crate) fn weight(&self) -> Result<f64, DecompositionError> {
        sequence_weight(&self.hops)
    }

    /// Price the last sell achieved, in human units. Gas is not accounted for.
    pub(crate) fn executed_price(&self) -> f64 {
        executed_price(&self.sell_amount, self.sell_token(), &self.buy_amount, self.buy_token())
    }

    // ===================== Selling =====================

    /// Sells along the chain, feeding hop `i`'s output into hop `i + 1`
    /// (`routes/sequential.py:113-154`).
    ///
    /// # Errors
    ///
    /// [`DecompositionError::SellAmountLimit`] with the limit expressed in
    /// [`SequenceRoute::sell_token`] units — a limit hit at an intermediate token is cast back so
    /// the caller can retry with `limit - 1` in the units it sells.
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

    /// Largest amount of [`SequenceRoute::sell_token`] the chain can absorb: the minimum over the
    /// hop limits, each cast back to sell-token units (`routes/sequential.py:176-185`).
    ///
    /// A hop with a limit of zero short-circuits the whole chain to zero. Cached until
    /// [`SequenceRoute::invalidate`].
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
            if best
                .as_ref()
                .is_none_or(|(current, _)| &cast < current)
            {
                best = Some((cast, pools));
            }
        }

        // `best` is always populated: the constructor rejects a chain without hops.
        let limit = best.unwrap_or_else(|| (BigUint::zero(), Vec::new()));
        self.limit_cache = Some(limit.clone());
        Ok(limit)
    }

    /// Converts an amount denominated in the token entering hop `hop_index` into sell-token units
    /// (`routes/sequential.py:187-197`).
    ///
    /// A linear approximation: it uses spot prices and so ignores the impact the preceding hops
    /// would suffer while actually pushing that amount through. defibot does the same, and the
    /// ported optimizers depend on the cast being cheap.
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
                self.hops[hop_index].sell_token(),
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
            decimal_scale(
                self.sell_token().decimals,
                self.hops[hop_index]
                    .sell_token()
                    .decimals,
            );
        (scaled.numer() / scaled.denom())
            .to_biguint()
            .ok_or_else(|| DecompositionError::InvalidStructure {
                reason: "cast to sell token produced a negative amount".to_string(),
            })
    }

    /// Hands the derived mid-prices to this chain and everything below it.
    pub(crate) fn set_prices(&mut self, prices: Arc<TokenGasPrices>) {
        self.prices = Some(Arc::clone(&prices));
        self.limit_cache = None;
        for hop in &mut self.hops {
            for child in hop.inner_mut() {
                child.set_prices(Arc::clone(&prices));
            }
        }
    }

    /// Drops this chain's cached limit and every cache below it.
    pub(crate) fn invalidate(&mut self) {
        self.limit_cache = None;
        for hop in &mut self.hops {
            hop.invalidate();
        }
    }
}
