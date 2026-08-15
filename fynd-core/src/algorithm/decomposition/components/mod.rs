//! Fixed-depth solution structures for the decomposition algorithm.
//!
//! Port of defibot's `FractalRoute` tree (`defibot/solver/order_solver/decomposition/routes/`).
//! defibot models a solution as an arbitrarily deep recursion of `SimpleRoute` (one pool),
//! `SequentialRoute` (hops in series) and `ParallelRoute` (alternatives in parallel). Years of
//! production use showed that solutions always collapse to the same three levels, so the recursion
//! is replaced here by a fixed structure:
//!
//! * [`DecompositionGraph`] — the outer `ParallelRoute` over one order.
//! * [`Branch`] — one outer split: a shared first [`Hop`] plus the parallel tails hanging off it.
//! * [`SequentialRoute`] — one tail: a token path with one [`Hop`] per leg.
//! * [`Hop`] — the inner `ParallelRoute` of `SimpleRoute`s at one leg, holding [`PoolRef`]s.
//!
//! A single direct pool is a one-branch graph whose branch has one pool in its head and no tails;
//! there is no special case for it.
//!
//! [`Branch`] is the level `_group_by_neighbour_token` (`order_solver.py:517-554`) exists to build.
//! It is what keeps a pool shared by several token paths from being allocated once per path: the
//! paths sharing a first hop become one branch, and that hop is sold once for all of them. See
//! [`Branch`] for the composition rules and for why they collapse to [`SequentialRoute`]'s when a
//! branch has a single tail.
//!
//! # Deviations from defibot
//!
//! * defibot's `EthereumToken` carries decimals and unit conversion, so a route can identify a
//!   token by symbol alone. Fynd's [`ProtocolSim`] takes `&Token` (it needs decimals to scale spot
//!   prices), so hops and routes hold `Token` rather than `Address`. `Token::address` is the
//!   identity in every comparison.
//! * defibot mixes on-chain integers with human-unit `Decimal`s. Everything here is on-chain
//!   `BigUint`; conversion to human units happens only where a price is produced.
//! * defibot's splits may be `None` (unsolved). Here an empty `splits` vector means unsolved and a
//!   non-empty one must match the pool count exactly.

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

mod branch;
mod error;
mod graph;
mod hop;
mod pool;
mod sequence;
mod split;

pub(crate) use branch::{Branch, BranchSide};
pub(crate) use error::DecompositionError;
pub(crate) use graph::DecompositionGraph;
pub(crate) use hop::Hop;
pub(crate) use pool::{PoolRef, SellLimitKind};
pub(crate) use sequence::SequentialRoute;
pub(crate) use split::{splits_sum, Fraction};

#[cfg(test)]
#[path = "../tests/components_tests.rs"]
mod tests;
