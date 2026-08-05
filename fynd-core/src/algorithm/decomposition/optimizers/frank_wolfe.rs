//! Frank-Wolfe split search.
//!
//! Not a defibot optimizer. It is the split loop of
//! [`PathFrankWolfeAlgorithm`](crate::algorithm::PathFrankWolfeAlgorithm) applied to a
//! decomposition solve's alternatives.
//!
//! The loop:
//!
//! 1. Rank the alternatives on what each buys net of gas at the whole amount, and give the best one
//!    everything.
//! 2. For each remaining alternative, line-search `step` over `[0, 1]`, the share moved out of
//!    everything already carrying flow and into this one.
//! 3. Take the step only if it beats leaving the alternative out.
//!
//! Existing shares are multiplied by `1 - step` rather than replaced, so an alternative that is
//! carrying flow always keeps some. [`PairComparison`](super::pair_comparison::PairComparison)
//! folds two at a time and its first pass steps by a half, so an alternative that can absorb a few
//! percent settles at zero and is never revisited.
//!
//! Unlike `PathFrankWolfeAlgorithm`, the market is not re-priced between steps: each alternative
//! owns private [`PoolRef`](crate::algorithm::decomposition::components::PoolRef) copies and is
//! sold against untouched state, so a split whose alternatives share a pool is overvalued and
//! [`sell_with_coupled_paths`](crate::algorithm::decomposition::solve) corrects the totals
//! afterwards.

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use tracing::debug;
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::{
    decomposition::{
        components::{DecompositionError, Fraction},
        optimizers::{
            decrease_until_sell, split_of, GasPrices, Sellable, SplitOptimizerT, SplitSolution,
        },
    },
    split_primitives::golden_section_search,
};

/// Evaluations spent on each alternative's line search.
///
/// `PathFrankWolfeConfig::line_search_evals` uses the same figure. One evaluation sells on every
/// alternative carrying flow, so the search costs `alternatives² × evaluations` sells at worst.
const LINE_SEARCH_EVALS: usize = 12;

/// Splits a sell amount by moving flow into one alternative at a time.
pub(crate) struct FrankWolfe;

impl SplitOptimizerT for FrankWolfe {
    fn optimize<S: Sellable>(
        &self,
        routes: &mut [S],
        sell_amount: &BigUint,
        gas_prices: &GasPrices,
    ) -> Result<SplitSolution, DecompositionError> {
        split_by_frank_wolfe(routes, sell_amount, gas_prices)
    }
}

/// # Errors
///
/// [`DecompositionError::InvalidStructure`] when `sell_amount` is zero — every share in the search
/// is a fraction of it. Any non-recoverable failure raised while selling is propagated.
fn split_by_frank_wolfe<S: Sellable>(
    routes: &mut [S],
    sell_amount: &BigUint,
    gas_prices: &GasPrices,
) -> Result<SplitSolution, DecompositionError> {
    if routes.is_empty() {
        return Ok(SplitSolution {
            sold: BigUint::zero(),
            bought: BigUint::zero(),
            splits: Vec::new(),
        });
    }
    if sell_amount.is_zero() {
        return Err(DecompositionError::InvalidStructure {
            reason: "cannot split a zero sell amount".to_string(),
        });
    }
    if routes.len() == 1 {
        let (bought, _) = decrease_until_sell(&mut routes[0], sell_amount)?;
        return Ok(SplitSolution {
            sold: routes[0].sell_amount().clone(),
            bought,
            splits: vec![Fraction::one()],
        });
    }

    let buy_token = routes[0].buy_token().clone();
    let ranked = rank(routes, sell_amount, gas_prices, &buy_token)?;
    let Some((&best, candidates)) = ranked.split_first() else {
        // Nothing could sell. Reset every alternative so the caller sees zero splits rather than
        // whatever the ranking sells left behind.
        for route in routes.iter_mut() {
            route.sell(&BigUint::zero())?;
        }
        return Ok(SplitSolution {
            sold: BigUint::zero(),
            bought: BigUint::zero(),
            splits: vec![Fraction::zero(); routes.len()],
        });
    };

    let mut shares = vec![0.0; routes.len()];
    shares[best] = 1.0;

    for &candidate in candidates {
        let step = search_step(routes, &shares, candidate, sell_amount, gas_prices, &buy_token)?;
        if step <= 0.0 {
            continue;
        }
        let stepped = with_step(&shares, candidate, step);
        let with_candidate = evaluate(routes, &stepped, sell_amount, gas_prices, &buy_token)?;
        let without_candidate = evaluate(routes, &shares, sell_amount, gas_prices, &buy_token)?;
        if with_candidate <= without_candidate {
            continue;
        }
        debug!(
            candidate,
            step,
            gain = %(&with_candidate - &without_candidate),
            "frank-wolfe moved flow into an alternative"
        );
        shares = stepped;
    }

    // The caller reads the split back off the alternatives.
    evaluate(routes, &shares, sell_amount, gas_prices, &buy_token)?;
    let sold = routes
        .iter()
        .map(Sellable::sell_amount)
        .fold(BigUint::zero(), |total, amount| total + amount);
    let splits = routes
        .iter()
        .map(|route| split_of(route.sell_amount(), sell_amount))
        .collect();
    let bought = routes
        .iter()
        .map(Sellable::buy_amount)
        .fold(BigUint::zero(), |total, amount| total + amount);
    Ok(SplitSolution { sold, bought, splits })
}

/// Line-searches the share to move into `candidate`, or zero when no step helps.
fn search_step<S: Sellable>(
    routes: &mut [S],
    shares: &[f64],
    candidate: usize,
    sell_amount: &BigUint,
    gas_prices: &GasPrices,
    buy_token: &Token,
) -> Result<f64, DecompositionError> {
    // The search wants an `f64` per trial, so a failed sell is reported as the worst possible
    // score and the error is carried out here rather than swallowed.
    let mut failure = None;
    let step = golden_section_search(
        |step| {
            let trial = with_step(shares, candidate, step);
            match evaluate(routes, &trial, sell_amount, gas_prices, buy_token) {
                Ok(net) => net
                    .to_f64()
                    .unwrap_or(f64::NEG_INFINITY),
                Err(error) => {
                    failure = Some(error);
                    f64::NEG_INFINITY
                }
            }
        },
        0.0,
        1.0,
        LINE_SEARCH_EVALS,
    );
    match failure {
        Some(error) => Err(error),
        None => Ok(step),
    }
}

/// `shares` with `step` of the total moved out of everything else and into `candidate`.
fn with_step(shares: &[f64], candidate: usize, step: f64) -> Vec<f64> {
    let mut stepped: Vec<f64> = shares
        .iter()
        .map(|share| share * (1.0 - step))
        .collect();
    stepped[candidate] = step;
    stepped
}

/// Sells `shares` of `sell_amount` on each alternative and returns the total bought net of gas.
///
/// Alternatives on a zero share are sold zero, which resets them — the search revisits shares
/// repeatedly and a stale sell would be counted by the caller as flow that is no longer there.
fn evaluate<S: Sellable>(
    routes: &mut [S],
    shares: &[f64],
    sell_amount: &BigUint,
    gas_prices: &GasPrices,
    buy_token: &Token,
) -> Result<BigInt, DecompositionError> {
    let mut bought = BigUint::zero();
    let mut gas = BigUint::zero();
    for (index, share) in shares.iter().enumerate() {
        let amount = fraction(*share).apply(sell_amount);
        if amount.is_zero() {
            routes[index].sell(&BigUint::zero())?;
            continue;
        }
        let (route_bought, route_gas) = decrease_until_sell(&mut routes[index], &amount)?;
        bought += route_bought;
        gas += route_gas;
    }
    let cost = gas_prices.cost_in_token(&gas, &buy_token.address);
    Ok(BigInt::from(bought) - BigInt::from(cost))
}

/// Ranks the alternatives on what they buy net of gas at the whole amount, best first.
///
/// Unsolved alternatives and ones that cannot cover their own gas are dropped.
fn rank<S: Sellable>(
    routes: &mut [S],
    sell_amount: &BigUint,
    gas_prices: &GasPrices,
    buy_token: &Token,
) -> Result<Vec<usize>, DecompositionError> {
    let mut scored = Vec::with_capacity(routes.len());
    for (index, route) in routes.iter_mut().enumerate() {
        if !route.solved() {
            continue;
        }
        let (bought, gas) = decrease_until_sell(route, sell_amount)?;
        let cost = gas_prices.cost_in_token(&gas, &buy_token.address);
        let net = BigInt::from(bought) - BigInt::from(cost);
        if !net.is_positive() {
            continue;
        }
        scored.push((index, net));
    }
    scored.sort_by(|left, right| right.1.cmp(&left.1));
    Ok(scored
        .into_iter()
        .map(|(index, _)| index)
        .collect())
}

/// A share as an exact split. Values outside `[0, 1]` and non-finite ones become zero.
fn fraction(share: f64) -> Fraction {
    if !(0.0..=1.0).contains(&share) {
        return Fraction::zero();
    }
    BigRational::from_float(share)
        .map(Fraction::new)
        .unwrap_or_else(Fraction::zero)
}

#[cfg(test)]
#[path = "../tests/frank_wolfe_tests.rs"]
mod tests;
