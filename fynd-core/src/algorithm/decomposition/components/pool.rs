//! One pool, and the state its trial sells leave behind.

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::components::*;

// ===================== PoolRef =====================

/// Cached result of selling one amount on one pool.
struct CachedSwap {
    buy_amount: BigUint,
    gas: BigUint,
    new_state: Box<dyn ProtocolSim>,
}

/// One pool inside a [`Hop`], with the amounts a sell on it produced.
///
/// Equivalent of defibot's `SimpleRoute` (`routes/simple.py`). `state` is the pre-trade simulation
/// state, `new_state` the post-trade state produced by the last sell — the quantity the optimizers
/// equalise reads off it.
/// What a pool's reported `get_limits` sell limit actually means.
///
/// The two kinds are not interchangeable, and treating them alike is what caps a constant-product
/// branch far below what it can trade. See [`PoolRef::sell_amount_limit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellLimitKind {
    /// The pool reverts above the reported limit, so it is a feasibility bound and must be
    /// respected. Concentrated liquidity runs out of indexed ticks; `0xf787…b91744` fails with
    /// `Ticks exceeded` at 900 GNO against a reported limit of 604.
    Enforced,
    /// The swap always succeeds at any size, so the reported limit is a *quality* heuristic and
    /// imposes no feasibility bound.
    ///
    /// tycho returns `2.162 * reserve_in` for constant-product pools — the input producing roughly
    /// 90% price impact (`cpmm_get_limits`). Nothing reverts above it: `0x3e84…1702c6` reports 520
    /// GNO and simulates 3500 GNO without complaint, which is the size `water_fill` actually
    /// trades there.
    Advisory,
}

impl SellLimitKind {
    /// Classifies a pool by the protocol system it was indexed under.
    ///
    /// Constant-product pools cannot revert on size, so their limit is advisory. Everything else
    /// is assumed to enforce its limit, which is the safe direction: over-respecting a limit costs
    /// output, under-respecting it produces a route that reverts on chain.
    pub(crate) fn for_protocol_system(protocol_system: &str) -> Self {
        match protocol_system {
            "uniswap_v2" | "sushiswap_v2" | "pancakeswap_v2" => Self::Advisory,
            _ => Self::Enforced,
        }
    }
}

/// The sell limit of a pool that cannot refuse one.
///
/// `u256::MAX`, which every on-chain token amount fits inside, so no comparison against a real
/// amount can ever bind. Limits are only ever compared, summed, minimised and cast — never
/// converted back to a `U256` or handed to a simulation — so the sums this produces at [`Hop`] and
/// [`DecompositionGraph`] level are free to exceed it.
fn unbounded_sell_limit() -> BigUint {
    (BigUint::from(1u8) << 256u32) - BigUint::from(1u8)
}

pub(crate) struct PoolRef {
    component_id: ComponentId,
    limit_kind: SellLimitKind,
    state: Box<dyn ProtocolSim>,
    depth: Option<BigUint>,
    new_state: Option<Box<dyn ProtocolSim>>,
    sell_amount: BigUint,
    buy_amount: BigUint,
    gas: BigUint,
    /// Probed on every sell simulation, keyed by an amount we produced ourselves.
    swap_cache: FxHashMap<BigUint, CachedSwap>,
    limit_cache: Option<BigUint>,
}

impl PoolRef {
    /// Trial sells this pool has memoised. Test-only: the cache is an optimisation, and the only
    /// thing worth asserting about it is that invalidation empties it.
    #[cfg(test)]
    pub(crate) fn cached_swaps(&self) -> usize {
        self.swap_cache.len()
    }

    /// Whether a sell amount is already memoised. Test-only, as above.
    #[cfg(test)]
    pub(crate) fn has_cached_swap(&self, amount: &BigUint) -> bool {
        self.swap_cache.contains_key(amount)
    }

    /// Whether the sell limit is memoised. Test-only, as above.
    #[cfg(test)]
    pub(crate) fn has_cached_limit(&self) -> bool {
        self.limit_cache.is_some()
    }

    /// Wraps a pool's simulation state as an untraded pool reference.
    ///
    /// `depth` is this pool's entry from
    /// [`ComponentDepths`](crate::derived::types::ComponentDepths) for the direction the enclosing
    /// [`Hop`] trades in — the largest input before the executed price slips past the
    /// configured threshold, in on-chain units of the hop's input token. It is defibot's
    /// *inertia* and cannot be derived from the simulation state alone, so it is supplied here
    /// rather than computed. Pass `None` when the derived store has no entry for the pool; see
    /// [`PoolRef::inertia`].
    ///
    /// `limit_kind` says whether this pool's reported sell limit is a hard bound or a heuristic;
    /// see [`SellLimitKind`].
    pub(crate) fn new(
        component_id: ComponentId,
        limit_kind: SellLimitKind,
        state: Box<dyn ProtocolSim>,
        depth: Option<BigUint>,
    ) -> Self {
        Self {
            component_id,
            limit_kind,
            state,
            depth,
            new_state: None,
            sell_amount: BigUint::zero(),
            buy_amount: BigUint::zero(),
            gas: BigUint::zero(),
            swap_cache: FxHashMap::default(),
            limit_cache: None,
        }
    }

    /// Component this pool belongs to.
    pub(crate) fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    /// Pre-trade simulation state.
    pub(crate) fn state(&self) -> &dyn ProtocolSim {
        self.state.as_ref()
    }

    /// Post-trade simulation state, or `None` if nothing has been sold on this pool.
    pub(crate) fn new_state(&self) -> Option<&dyn ProtocolSim> {
        self.new_state.as_deref()
    }

    /// Amount sold on this pool by the last [`PoolRef::sell`].
    pub(crate) fn sell_amount(&self) -> &BigUint {
        &self.sell_amount
    }

    /// Amount bought on this pool by the last [`PoolRef::sell`].
    pub(crate) fn buy_amount(&self) -> &BigUint {
        &self.buy_amount
    }

    /// Gas the last [`PoolRef::sell`] reported.
    pub(crate) fn gas(&self) -> &BigUint {
        &self.gas
    }

    /// Price the last [`PoolRef::sell`] achieved, in human units. Gas is not accounted for.
    ///
    /// The tokens are passed in because a pool does not know which direction it was traded in
    /// (`routes/interface.py:117-127`).
    pub(crate) fn executed_price(&self, token_in: &Token, token_out: &Token) -> f64 {
        executed_price(&self.sell_amount, token_in, &self.buy_amount, token_out)
    }

    /// Spot price of `token_out` per `token_in` at the pre-trade state.
    pub(crate) fn route_price(
        &self,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<f64, DecompositionError> {
        self.state
            .spot_price(token_in, token_out)
            .map_err(|source| DecompositionError::Simulation {
                component: self.component_id.clone(),
                source,
            })
    }

    /// Trading fee as a fraction of the input.
    pub(crate) fn fee(&self) -> f64 {
        self.state.fee()
    }

    /// Spot price net of the trading fee.
    pub(crate) fn marginal_price(
        &self,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<f64, DecompositionError> {
        Ok(self.route_price(token_in, token_out)? * (1.0 - self.fee()))
    }

    /// Marginal price at the post-trade state, or `None` if this pool was not sold on.
    pub(crate) fn new_marginal_price(&self, token_in: &Token, token_out: &Token) -> Option<f64> {
        let Some(new_state) = self.new_state.as_ref() else {
            return None;
        };
        let Ok(price) = new_state.spot_price(token_in, token_out) else {
            return None;
        };
        Some(price * (1.0 - new_state.fee()))
    }

    /// Liquidity depth of this pool, in human units of `token_in`.
    ///
    /// defibot's `Inertia` (`defibot/swaps/interfaces.py:80-95`, computed at
    /// `data_pipeline/jobs/inertia_calculator.py:28-60`) is the trade size that moves the executed
    /// price down to `route_price * (1 - depth - fee)` — how much can be sold before the price
    /// slips by a configured threshold. Fynd computes exactly that in
    /// [`PoolDepthComputation`](crate::derived::computations::pool_depth::PoolDepthComputation),
    /// so the value is taken from the depth supplied to [`PoolRef::new`]. It is deliberately *not*
    /// the pool-exhaustion limit from `get_limits`: the two differ by orders of magnitude and rank
    /// concentrated-liquidity pools differently, and inertia drives candidate ranking through
    /// [`PoolRef::weight`].
    ///
    /// Returns [`MISSING_DEPTH_INERTIA`] when no depth was supplied — depths can legitimately be
    /// absent for a pool. This mirrors defibot falling back to `1` (`routes/simple.py:57-68`), and
    /// is the only fallback: there is no second path that substitutes a different quantity.
    pub(crate) fn inertia(&self, token_in: &Token) -> f64 {
        let Some(depth) = self.depth.as_ref() else {
            return MISSING_DEPTH_INERTIA;
        };
        let Some(depth) = depth.to_f64() else {
            return MISSING_DEPTH_INERTIA;
        };
        let scaled = depth / 10f64.powi(token_in.decimals as i32);
        if scaled.is_finite() {
            scaled
        } else {
            MISSING_DEPTH_INERTIA
        }
    }

    /// Ranking score: `inertia * (1 - fee) * route_price` (`routes/simple.py:190-201`).
    pub(crate) fn weight(
        &self,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<f64, DecompositionError> {
        Ok(self.inertia(token_in) * (1.0 - self.fee()) * self.route_price(token_in, token_out)?)
    }

    /// Largest amount of `token_in` this pool can absorb, in on-chain units.
    ///
    /// For a [`SellLimitKind::Enforced`] pool this is tycho's reported sell limit. For a
    /// [`SellLimitKind::Advisory`] one it is [`unbounded_sell_limit`]: the swap cannot revert on
    /// size, so there is nothing to bound, and the reported figure is a price-impact heuristic
    /// that has no business acting as a feasibility cap. Ranking still penalises the bad prices
    /// such a trade earns, through [`PoolRef::weight`] and the optimizer's price comparison —
    /// which is where a *quality* signal belongs.
    ///
    /// This diverges from defibot, whose constant-product limit is
    /// `spot(buy, sell) * reserves[buy]`, algebraically just `reserve_in` and so *tighter* than
    /// tycho's `2.162 * reserve_in`. Neither reaches the sizes these pools actually trade at.
    ///
    /// Cached until [`PoolRef::invalidate`] (`routes/interface.py:303-312`).
    pub(crate) fn sell_amount_limit(
        &mut self,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<BigUint, DecompositionError> {
        if let Some(limit) = self.limit_cache.as_ref() {
            return Ok(limit.clone());
        }

        let limit = match self.limit_kind {
            SellLimitKind::Advisory => unbounded_sell_limit(),
            SellLimitKind::Enforced => {
                let (sell_limit, _) = self
                    .state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .map_err(|source| DecompositionError::Simulation {
                        component: self.component_id.clone(),
                        source,
                    })?;
                sell_limit
            }
        };
        self.limit_cache = Some(limit.clone());
        Ok(limit)
    }

    /// Sells `amount` of `token_in` on this pool, recording the resulting state and amounts.
    ///
    /// Selling zero resets the pool and clears the post-trade state
    /// (`routes/simple.py:124-129`). Results are cached per sell amount
    /// (`routes/simple.py:131-137`, `:178`).
    ///
    /// # Errors
    ///
    /// [`DecompositionError::SellAmountLimit`] when `amount` exceeds the pool's trade limit, and
    /// [`DecompositionError::Simulation`] when the pool math fails or panics.
    pub(crate) fn sell(
        &mut self,
        amount: &BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<(BigUint, BigUint), DecompositionError> {
        if amount.is_zero() {
            self.sell_amount = BigUint::zero();
            self.buy_amount = BigUint::zero();
            self.gas = BigUint::zero();
            self.new_state = None;
            return Ok((BigUint::zero(), BigUint::zero()));
        }

        if let Some(cached) = self.swap_cache.get(amount) {
            self.sell_amount = amount.clone();
            self.buy_amount = cached.buy_amount.clone();
            self.gas = cached.gas.clone();
            self.new_state = Some(cached.new_state.clone_box());
            return Ok((self.buy_amount.clone(), self.gas.clone()));
        }

        let limit = self.sell_amount_limit(token_in, token_out)?;
        if amount > &limit {
            return Err(DecompositionError::SellAmountLimit {
                limit,
                token: token_in.address.clone(),
                pools: vec![self.component_id.clone()],
            });
        }

        let result = self
            .state
            .get_amount_out_guarded(amount.clone(), token_in, token_out)
            .map_err(|source| DecompositionError::Simulation {
                component: self.component_id.clone(),
                source,
            })?;

        self.sell_amount = amount.clone();
        self.buy_amount = result.amount.clone();
        self.gas = result.gas.clone();
        self.new_state = Some(result.new_state.clone_box());
        self.swap_cache.insert(
            amount.clone(),
            CachedSwap {
                buy_amount: result.amount.clone(),
                gas: result.gas.clone(),
                new_state: result.new_state,
            },
        );

        Ok((result.amount, result.gas))
    }

    /// Drops the swap and limit caches after the underlying pool state changed
    /// (`routes/simple.py:259-264`).
    pub(crate) fn invalidate(&mut self) {
        self.swap_cache.clear();
        self.limit_cache = None;
    }

    /// Replaces the pre-trade state and drops every cache derived from it
    /// (`routes/simple.py:259-264`).
    ///
    /// The post-trade state and the recorded amounts are deliberately left alone: callers replay a
    /// branch against the liquidity earlier branches consumed and then read back what each branch
    /// sold, so wiping the results here would erase the answer being computed.
    pub(crate) fn update_state(&mut self, state: Box<dyn ProtocolSim>) {
        self.state = state;
        self.invalidate();
    }
}
