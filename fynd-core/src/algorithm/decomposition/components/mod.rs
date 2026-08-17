//! Fixed-depth solution structures for the decomposition algorithm.
//!
//! Port of defibot's `FractalRoute` tree (`defibot/solver/order_solver/decomposition/routes/`).
//! defibot models a solution as an arbitrarily deep recursion of `SimpleRoute` (one pool),
//! `SequentialRoute` (hops in series) and `ParallelRoute` (alternatives in parallel). The same
//! three shapes are here, named rather than nested without limit:
//!
//! * [`DecompositionGraph`] — the outer parallel split over one order.
//! * [`SequenceRoute`] — hops in series. One branch of the graph, and also one tail of a grouped
//!   branch.
//! * [`ParallelRoute`] — alternatives in parallel: the pools of one leg, or the tails of a grouped
//!   branch.
//! * [`Pool`] — one pool traded in one direction, defibot's `SimpleRoute`.
//! * [`Route`] — which of the two a [`ParallelRoute`]'s alternatives are.
//!
//! A single direct pool is a one-branch graph whose branch is a one-hop chain over one pool; there
//! is no special case for it.
//!
//! # The grouped branch
//!
//! `_group_by_neighbour_token` (`order_solver.py:517-554`) exists to stop a pool shared by several
//! token paths from being allocated once per path. The paths sharing a hop become one branch, and
//! that hop is sold once for all of them.
//!
//! Such a branch is a two-hop [`SequenceRoute`]: the shared hop, and a [`ParallelRoute`] whose
//! alternatives are the tails. Which of the two comes first is the grouping's choice — leading when
//! the paths share their first hop, trailing when they share their last. Every composition rule
//! collapses to the plain chain's when there is exactly one tail, so grouping costs nothing on
//! branches that share nothing.
//!
//! # Deviations from defibot
//!
//! * defibot's `EthereumToken` carries decimals and unit conversion, so a route can identify a
//!   token by symbol alone. Fynd's [`ProtocolSim`] takes `&Token` (it needs decimals to scale spot
//!   prices), so components hold tokens rather than addresses — as `Arc<Token>`, which is how
//!   `MarketState` keeps them, so passing one around is a pointer rather than a `String` symbol and
//!   a `Vec<Option<TransferCost>>`.
//! * defibot mixes on-chain integers with human-unit `Decimal`s. Everything here is on-chain
//!   `BigUint`; conversion to human units happens only where a price is produced.
//! * defibot's splits may be `None` (unsolved). Here an empty `splits` vector means unsolved and a
//!   non-empty one must match the alternative count exactly.

use std::sync::Arc;

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{One, ToPrimitive, Zero};
use rustc_hash::FxHashMap;
use tracing::debug;
use tycho_simulation::tycho_core::{
    models::token::Token,
    simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
};

use crate::{
    algorithm::sim_guard::GuardedProtocolSim, derived::types::TokenGasPrices, types::ComponentId,
};

/// Converts `amount` of `from` into `to` units through the derived mid-prices.
///
/// Every token in [`TokenGasPrices`] is priced against one numeraire — the gas token — so any pair
/// converts in a single step: into the numeraire and back out. That is what makes a branch's sell
/// limit a flow calculation, where parallel alternatives sum and a sequence takes its tightest
/// hop. Chaining each hop's spot price instead compounds every hop's error, and averaging prices
/// across a set of unrelated routes is not a price at all.
///
/// The mid-prices map wei to on-chain token units (see `GasPrices::cost_in_token`), so decimals
/// are already accounted for and the result is on-chain units of `to`.
///
/// Returns `None` when either token is unpriced or a price is degenerate, which leaves the caller
/// on its spot-price fallback. `DecompositionAlgorithm::excluded_tokens` drops unpriced tokens from
/// candidate discovery, so on a solve with derived prices this should not happen.
pub(crate) fn convert_through_numeraire(
    prices: &TokenGasPrices,
    amount: &BigUint,
    from: &Token,
    to: &Token,
) -> Option<BigUint> {
    if from.address == to.address {
        return Some(amount.clone());
    }
    let from_price = prices.get(&from.address)?;
    let to_price = prices.get(&to.address)?;
    if from_price.numerator.is_zero() || to_price.denominator.is_zero() {
        return None;
    }
    Some(
        amount * &from_price.denominator * &to_price.numerator /
            (&from_price.numerator * &to_price.denominator),
    )
}

/// Largest denominator a split may have.
///
/// Mirrors `SPLIT_PRECISION` in `defibot/solver/order_solver/constants.py:1-5`. Splits are
/// multiplied by large integer sell amounts, so they are kept as exact rationals; the denominator
/// bound keeps them representable on-chain and stops continued fractions from growing without
/// bound.
pub(crate) const SPLIT_PRECISION: u64 = 1_000_000;

/// Inertia assumed for a pool with no depth entry in the derived store.
///
/// defibot's precedent is a bare `except: return 1` (`routes/simple.py:57-68`). Inertia only ever
/// feeds candidate ranking and pruning, so a wrong constant degrades ordering rather than
/// producing a wrong trade.
pub(crate) const MISSING_DEPTH_INERTIA: f64 = 1.0;

// ===================== Shared helpers =====================

/// `10^(target_decimals - source_decimals)` as an exact rational.
fn decimal_scale(target_decimals: u32, source_decimals: u32) -> BigRational {
    let power = BigInt::from(10u8).pow(target_decimals.abs_diff(source_decimals));
    if target_decimals >= source_decimals {
        BigRational::from(power)
    } else {
        BigRational::new(BigInt::one(), power)
    }
}

/// Realised price of a trade in human units (`routes/interface.py:117-127`).
///
/// Returns `0.0` on a zero buy amount and on a zero sell amount, where defibot's `Decimal`
/// division raises `DivisionByZero` and is caught.
fn executed_price(
    sell_amount: &BigUint,
    sell_token: &Token,
    buy_amount: &BigUint,
    buy_token: &Token,
) -> f64 {
    if buy_amount.is_zero() || sell_amount.is_zero() {
        return 0.0;
    }
    let ratio =
        BigRational::new(BigInt::from(buy_amount.clone()), BigInt::from(sell_amount.clone())) *
            decimal_scale(sell_token.decimals, buy_token.decimals);
    ratio.to_f64().unwrap_or(0.0)
}

mod error;
mod graph;
mod pool;
mod route;
mod sequence;
mod split;

pub(crate) use error::DecompositionError;
pub(crate) use graph::DecompositionGraph;
pub(crate) use pool::{Pool, SellLimitKind};
pub(crate) use route::Route;
pub(crate) use sequence::{sequence_weight, SequenceRoute};
pub(crate) use split::ParallelRoute;

pub(crate) use crate::algorithm::decomposition::models::Fraction;

#[cfg(test)]
#[path = "../tests/components_tests.rs"]
mod tests;

/// Sum of a split vector as an exact rational.
pub(crate) fn splits_sum(splits: &[Fraction]) -> BigRational {
    let mut total = BigRational::zero();
    for split in splits {
        total += split.as_ratio();
    }
    total
}
