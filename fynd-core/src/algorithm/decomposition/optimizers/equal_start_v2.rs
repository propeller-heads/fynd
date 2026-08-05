//! Equal-start split search, second version.
//!
//! Port of `defibot/solver/order_solver/decomposition/optimizers/equal_start_v2.py`, which reuses
//! several helpers from `optimizers/equal_start.py`. The clearest statement of the idea is v1's
//! module docstring (`equal_start.py:1-20`):
//!
//! > Starts with splitting the sell amount equally among the routes. Then iteratively decreases the
//! > share of the worst route, while increasing the share of the best route. [...] This algorithm
//! > converges towards equalizing marginal prices. We identified that this yields the best executed
//! > price.
//!
//! Equalising post-trade marginal prices is the first-order condition for maximising output across
//! parallel concave curves: while one alternative still prices a marginal unit higher than another,
//! moving flow towards it buys more. The search is therefore a gradient walk on the split simplex
//! with a fixed step, not a line search like [`PairComparison`](super::pair_comparison).
//!
//! # Multi-resolution
//!
//! [`find_best_splits`] runs once per entry of [`STEPS`], each pass seeded with the previous pass's
//! result (`equal_start_v2.py:85-93`). Big steps eliminate hopeless alternatives in a handful of
//! iterations; small steps then refine the survivors, and a pass that cannot improve stops on its
//! first iteration. The winner across passes is chosen on **bought minus gas**, not gross bought
//! (`:94-100`) — a finer split is free to spread the order over more pools, and only the net
//! comparison prices that against the gas it costs.
//!
//! # Deviations from defibot
//!
//! * `iteration_strategy` selects the ranking metric. defibot reads it from global config **at
//!   module import time** (`equal_start_v2.py:13-15`), so the process cannot be reconfigured and a
//!   test cannot exercise the other branch. It is a constructor parameter here — see
//!   [`RankingMetric`].
//! * defibot's `max_splits` parameter is an *iteration budget*, colliding in name with the solver's
//!   route-count cap of the same name. It is [`EqualStartV2::max_move_iterations`] here.
//! * defibot leaves the budget unset in production, relying on the `visited` set alone to
//!   terminate. The default here is a finite budget: the walk visits a lattice whose size is
//!   exponential in the number of alternatives, and this optimizer runs on a worker thread that
//!   owns no deadline of its own.
//! * defibot marks a route "exhausted"/"saturated" by setting ad-hoc attributes on the pydantic
//!   route model and deleting them afterwards (`equal_start.py:284-286`, only legal under
//!   `Extra.allow`). Both live in local vectors here; neither is a property of a route.
//! * An alternative that is not [`Sellable::solved`] is skipped rather than sold on. defibot would
//!   raise from inside `ParallelRoute.sell` (`routes/parallel.py:179-180`); the solver never passes
//!   one, so the guard costs nothing and keeps a structural error from becoming an unrecoverable
//!   optimize failure.
//! * `_plot` (`equal_start.py:431-478`) and `_raise_if_no_indices` (`:243-250`) are not ported. The
//!   latter raises `NotImplementedError` for a case its author could not construct; an empty
//!   candidate list simply stops the search here.
//! * `_sell_by_splits` and `_round_splits` (`equal_start.py:327-372`, `:289-302`) are v1-only. v2
//!   imports exactly `_argsort`, `IterationRecord` and `_get_executed_price` from v1
//!   (`equal_start_v2.py:10`) and sizes its own sells inline (`:133-137`), so porting them would
//!   have produced two functions with no caller. Their jobs are covered: the running `sell_amount -
//!   total_sold` clamp is what stops rounding from overshooting the order, and exact rationals make
//!   v1's float-repair rounding unnecessary.

use std::collections::BTreeSet;

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{One, ToPrimitive, Zero};
use tracing::debug;
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::{
    components::{DecompositionError, Fraction},
    optimizers::{
        decrease_until_sell, split_of, GasPrices, Sellable, SplitOptimizerT, SplitSolution,
    },
};

/// Fractions of the order moved per iteration, coarsest first (`equal_start_v2.py:66-74`).
const STEPS: [(i64, i64); 7] = [(1, 2), (1, 5), (1, 10), (1, 50), (1, 100), (1, 200), (1, 500)];

/// Iterations one [`find_best_splits`] pass may run when no budget is configured.
///
/// defibot's default is unbounded (`equal_start_v2.py:24`, `:116`); see the module docs for why
/// this port bounds it. Sized so that a pass over a normal alternative count converges long before
/// the cap, and only a pathological walk ever sees it.
const DEFAULT_MAX_MOVE_ITERATIONS: usize = 500;

/// Which price the search ranks alternatives by (`equal_start_v2.py:172-176`).
///
/// defibot's `optimizer_config.iteration_strategy`, which it resolves **once at module import**
/// (`equal_start_v2.py:13-15`) — a process cannot be reconfigured and neither branch can be tested
/// against the other. It is chosen per optimizer instance here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RankingMetric {
    /// Post-trade marginal price. The quantity the walk equalises, and defibot's configured value
    /// in both configurations (`propeller-solver-core/core/defibot.yaml:638`).
    #[default]
    MarginalPrice,
    /// Price the alternative's last sell achieved, net of gas.
    ExecutedPrice,
}

/// Splits a sell amount by starting every alternative equal and moving flow to the best one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EqualStartV2 {
    ranking_metric: RankingMetric,
    max_move_iterations: Option<usize>,
}

impl EqualStartV2 {
    /// The optimizer ranking on `ranking_metric`, with this port's default iteration budget.
    pub(crate) fn new(ranking_metric: RankingMetric) -> Self {
        Self { ranking_metric, max_move_iterations: Some(DEFAULT_MAX_MOVE_ITERATIONS) }
    }

    /// Caps how many moves one resolution pass may make, or lifts the cap entirely with `None`.
    ///
    /// This is defibot's `max_splits` optimizer argument, which is **not** the solver's
    /// route-count cap of the same name — see [`DecompositionConfig::max_parallel_routes`].
    ///
    /// [`DecompositionConfig::max_parallel_routes`]: crate::algorithm::decomposition::DecompositionConfig::max_parallel_routes
    #[cfg(test)]
    pub(crate) fn with_max_move_iterations(mut self, iterations: Option<usize>) -> Self {
        self.max_move_iterations = iterations;
        self
    }
}

impl SplitOptimizerT for EqualStartV2 {
    fn optimize<S: Sellable>(
        &self,
        routes: &mut [S],
        sell_amount: &BigUint,
        gas_prices: &GasPrices,
    ) -> Result<SplitSolution, DecompositionError> {
        split_equal_start_v2(self, routes, sell_amount, gas_prices)
    }
}

/// Entry point (`equal_start_v2.py:20-107`).
///
/// # Errors
///
/// [`DecompositionError::InvalidStructure`] when `sell_amount` is zero — every split is a ratio
/// over it, and defibot raises `ZeroDivisionError` on the same input (`:255`). Any non-recoverable
/// failure raised while selling is propagated.
fn split_equal_start_v2<S: Sellable>(
    optimizer: &EqualStartV2,
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

    let mut splits = initial_splits(routes.len());
    // (net of gas, splits, gross bought) per resolution pass (`:83`, `:97`).
    let mut passes: Vec<(BigInt, Vec<Fraction>, BigUint)> = Vec::with_capacity(STEPS.len());

    let buy_token = routes[0].buy_token().clone();
    for step in steps() {
        let pass = find_best_splits(optimizer, routes, sell_amount, &splits, gas_prices, &step)?;
        splits = pass.splits;
        let cost = gas_prices.cost_in_token(&pass.gas, &buy_token.address);
        let net = BigInt::from(pass.bought.clone()) - BigInt::from(cost);
        debug!(
            step = %step,
            iterations = pass.iterations,
            bought = %pass.bought,
            net = %net,
            "equal start pass"
        );
        passes.push((net, splits.clone(), pass.bought));
    }

    // `max` keeps the first maximal element, so a later pass has to strictly beat the incumbent —
    // which is what stops a finer split from being taken for free (`:100`).
    let mut best = &passes[0];
    for pass in &passes[1..] {
        if pass.0 > best.0 {
            best = pass;
        }
    }
    let best_splits =
        if best.2.is_zero() { vec![Fraction::zero(); routes.len()] } else { best.1.clone() };

    let effective = effective_splits(routes, sell_amount, &best_splits)?;
    let mut sold = BigUint::zero();
    for route in routes.iter() {
        sold += route.sell_amount();
    }

    Ok(SplitSolution { sold, bought: effective.bought, splits: effective.splits })
}

/// The step schedule as exact rationals.
fn steps() -> impl Iterator<Item = BigRational> {
    STEPS
        .into_iter()
        .map(|(numerator, denominator)| {
            BigRational::new(BigInt::from(numerator), BigInt::from(denominator))
        })
}

/// An equal split per alternative, with the rounding remainder pushed onto the first non-zero one
/// (`equal_start_v2.py:75-81`).
///
/// The remainder is zero whenever `1/count` is exactly representable under
/// [`SPLIT_PRECISION`](crate::algorithm::decomposition::components::SPLIT_PRECISION), which is
/// every alternative count a solve can produce. It is carried anyway so that the vector sums to one
/// by construction rather than by assumption.
fn initial_splits(count: usize) -> Vec<Fraction> {
    let share = Fraction::new(BigRational::new(BigInt::one(), BigInt::from(count)));
    let mut splits = vec![share; count];
    let mut total = BigRational::zero();
    for split in &splits {
        total += split.as_ratio();
    }
    let remainder = BigRational::one() - total;
    if remainder.is_zero() {
        return splits;
    }
    for split in &mut splits {
        if !split.is_zero() {
            *split = Fraction::new(split.as_ratio() + &remainder);
            break;
        }
    }
    splits
}

/// What one resolution pass converged on.
struct Pass {
    bought: BigUint,
    gas: BigUint,
    splits: Vec<Fraction>,
    /// Split vectors the pass evaluated before it ran out of unvisited states or of budget.
    ///
    /// Always one for [`effective_splits`], which evaluates a vector it is handed. Logged per
    /// pass: it is the only visible measure of what the saturation rule and the `visited` set
    /// do, since both exist to stop the walk from re-treading ground.
    iterations: usize,
}

/// The state one move iteration reads off the alternatives.
struct Evaluation {
    /// Total bought net of gas, the quantity the search maximises.
    total_net: BigInt,
    /// Per-alternative executed price net of gas, `None` when nothing was sold.
    executed_prices: Vec<Option<f64>>,
    /// Per-alternative post-trade marginal price, `None` when the alternative was not sold on.
    marginal_prices: Vec<Option<f64>>,
    /// Whether each alternative may be sold on at all.
    usable: Vec<bool>,
}

/// Walks the split simplex at one resolution (`equal_start_v2.py:110-224`).
///
/// Each iteration sells the current splits, records what they bought net of gas, then moves one
/// `step` of flow from the worst alternative to the best. A move that loses is reverted and its
/// receiver marked saturated (`:164-167`), so the second-best receives next. The search stops when
/// no unvisited split vector is reachable, or when the iteration budget runs out.
fn find_best_splits<S: Sellable>(
    optimizer: &EqualStartV2,
    routes: &mut [S],
    sell_amount: &BigUint,
    start_splits: &[Fraction],
    gas_prices: &GasPrices,
    step: &BigRational,
) -> Result<Pass, DecompositionError> {
    let mut splits = start_splits.to_vec();
    let mut visited: BTreeSet<Vec<Fraction>> = BTreeSet::new();
    let mut records: Vec<(Vec<Fraction>, BigInt)> = Vec::new();
    let mut saturated = vec![false; routes.len()];
    let mut previous: Option<(Vec<Fraction>, BigInt)> = None;
    let mut last_receiver: Option<usize> = None;
    let mut iterations = 0usize;

    while optimizer
        .max_move_iterations
        .is_none_or(|budget| iterations <= budget)
    {
        iterations += 1;
        let evaluation = evaluate(routes, sell_amount, &splits, gas_prices)?;

        if visited.insert(splits.clone()) {
            records.push((splits.clone(), evaluation.total_net.clone()));
        }

        // The move lost. Undo it and never hand flow to that receiver again — defibot reverts to
        // the previous splits without re-selling, and the next iteration restores the routes.
        if let Some((previous_splits, previous_net)) = previous.as_ref() {
            if &evaluation.total_net < previous_net {
                let Some(receiver) = last_receiver else { break };
                saturated[receiver] = true;
                splits = previous_splits.clone();
                continue;
            }
        }
        previous = Some((splits.clone(), evaluation.total_net.clone()));

        let ranking = match optimizer.ranking_metric {
            RankingMetric::MarginalPrice => &evaluation.marginal_prices,
            RankingMetric::ExecutedPrice => &evaluation.executed_prices,
        };
        let senders = worst_route_indexes(routes, &splits, sell_amount, ranking, &evaluation);
        let receivers =
            best_route_indexes(routes, &splits, sell_amount, ranking, &evaluation, &saturated)?;

        let Some((next_splits, receiver)) =
            next_move(&splits, &senders, &receivers, step, &visited)
        else {
            break;
        };
        splits = next_splits;
        last_receiver = Some(receiver);
    }

    // Every visited vector is a candidate, not just the last: the walk is free to pass through a
    // worse state on its way, and the revert rule only prevents it from *continuing* from one.
    let mut best = &records[0];
    for record in &records[1..] {
        if record.1 > best.1 {
            best = record;
        }
    }
    let effective = effective_splits(routes, sell_amount, &best.0)?;

    Ok(Pass { iterations, ..effective })
}

/// Sells the current splits on every alternative and reads back what the ranking needs
/// (`equal_start_v2.py:126-151`).
///
/// The per-alternative amount is clamped to what is left of the order, so rounding can never push
/// the total above the order however the splits landed (`:134-136`).
fn evaluate<S: Sellable>(
    routes: &mut [S],
    sell_amount: &BigUint,
    splits: &[Fraction],
    gas_prices: &GasPrices,
) -> Result<Evaluation, DecompositionError> {
    let sell_token = routes[0].sell_token().clone();
    let buy_token = routes[0].buy_token().clone();

    let mut total_net = BigInt::zero();
    let mut total_sold = BigUint::zero();
    let mut executed_prices = Vec::with_capacity(routes.len());
    let mut marginal_prices = Vec::with_capacity(routes.len());
    let mut usable = Vec::with_capacity(routes.len());

    for (index, route) in routes.iter_mut().enumerate() {
        if !route.solved() {
            executed_prices.push(None);
            marginal_prices.push(None);
            usable.push(false);
            continue;
        }
        let remaining = sell_amount - &total_sold;
        let target = splits[index]
            .apply(sell_amount)
            .min(remaining);
        let (bought, gas) = decrease_until_sell(route, &target)?;
        let cost = gas_prices.cost_in_token(&gas, &buy_token.address);
        let net = BigInt::from(bought) - BigInt::from(cost);

        total_net += &net;
        total_sold += route.sell_amount();
        executed_prices.push(net_executed_price(
            route.sell_amount(),
            &net,
            &sell_token,
            &buy_token,
        ));
        marginal_prices.push(route.new_marginal_price()?);
        usable.push(true);
    }

    Ok(Evaluation { total_net, executed_prices, marginal_prices, usable })
}

/// The splits each alternative actually sold, and what they bought (`equal_start_v2.py:227-257`).
///
/// An alternative promised 0.3 that could only absorb 0.2 ends on 0.2, so **the result need not sum
/// to one**: the shortfall is how the optimizer reports that the alternatives could not take the
/// whole order (`optimizers/interface.py:26-31`).
fn effective_splits<S: Sellable>(
    routes: &mut [S],
    sell_amount: &BigUint,
    splits: &[Fraction],
) -> Result<Pass, DecompositionError> {
    let mut bought = BigUint::zero();
    let mut gas = BigUint::zero();
    let mut effective = Vec::with_capacity(routes.len());

    for (index, route) in routes.iter_mut().enumerate() {
        if !route.solved() {
            effective.push(Fraction::zero());
            continue;
        }
        if splits[index].is_zero() {
            route.sell(&BigUint::zero())?;
            effective.push(Fraction::zero());
            continue;
        }
        let (route_bought, route_gas) =
            decrease_until_sell(route, &splits[index].apply(sell_amount))?;
        bought += route_bought;
        gas += route_gas;
        effective.push(split_of(route.sell_amount(), sell_amount));
    }

    Ok(Pass { bought, gas, splits: effective, iterations: 1 })
}

/// Alternatives ordered worst first, as candidates to take flow *from*
/// (`equal_start_v2.py:260-285`).
///
/// An alternative that could not sell its allocation ranks by the negative shortfall, which is
/// below any price and so sorts it to the very worst (`:269-276`) — the search wants that flow
/// somewhere it can be used. An alternative already at zero has nothing to give and is excluded
/// (`:277-281`).
fn worst_route_indexes<S: Sellable>(
    routes: &[S],
    splits: &[Fraction],
    sell_amount: &BigUint,
    ranking: &[Option<f64>],
    evaluation: &Evaluation,
) -> Vec<usize> {
    let mut values = Vec::with_capacity(routes.len());
    for (index, route) in routes.iter().enumerate() {
        if !evaluation.usable[index] {
            values.push(None);
            continue;
        }
        let allocation = splits[index].apply(sell_amount);
        if route.sell_amount() < &allocation {
            values.push(shortfall(route.sell_amount(), &allocation));
        } else if splits[index].is_zero() {
            values.push(None);
        } else {
            values.push(ranking[index]);
        }
    }
    argsort_ascending(&values)
}

/// Alternatives ordered best first, as candidates to give flow *to*
/// (`equal_start_v2.py:288-321`).
///
/// An alternative already holding the whole order cannot receive (`:307-311`). One that could not
/// sell its allocation is ranked by the price it achieved against the amount it was *asked* for, so
/// it scores below one that filled (`:298-306`). One that has not been sold on has no post-trade
/// price, so its pre-trade [`Sellable::marginal_price`] stands in — the search is trying to find
/// out what it would do with some flow (`:312-319`).
fn best_route_indexes<S: Sellable>(
    routes: &[S],
    splits: &[Fraction],
    sell_amount: &BigUint,
    ranking: &[Option<f64>],
    evaluation: &Evaluation,
    saturated: &[bool],
) -> Result<Vec<usize>, DecompositionError> {
    let sell_token = routes[0].sell_token().clone();
    let buy_token = routes[0].buy_token().clone();

    let mut values = Vec::with_capacity(routes.len());
    for (index, route) in routes.iter().enumerate() {
        if !evaluation.usable[index] {
            values.push(None);
            continue;
        }
        let allocation = splits[index].apply(sell_amount);
        if route.sell_amount() < &allocation {
            values.push(net_executed_price(
                &allocation,
                &BigInt::from(route.buy_amount().clone()),
                &sell_token,
                &buy_token,
            ));
        } else if splits[index].as_ratio().is_one() {
            values.push(None);
        } else if let Some(price) = ranking[index] {
            values.push(Some(price));
        } else {
            values.push(Some(route.marginal_price()?));
        }
    }

    let mut ordered = argsort_ascending_masked(&values, saturated);
    ordered.reverse();
    Ok(ordered)
}

/// The first unvisited move from a sender to a receiver (`equal_start_v2.py:190-216`).
///
/// Senders are tried worst first and receivers best first. The receiver scan stops — rather than
/// skipping — as soon as it reaches the sender itself, because every receiver past that point is
/// ranked below the alternative the flow is being taken from.
///
/// Returns the new split vector and the receiver that earned it, or `None` when the search has
/// nowhere left to go.
fn next_move(
    splits: &[Fraction],
    senders: &[usize],
    receivers: &[usize],
    step: &BigRational,
    visited: &BTreeSet<Vec<Fraction>>,
) -> Option<(Vec<Fraction>, usize)> {
    for &sender in senders {
        for &receiver in receivers {
            if receiver == sender {
                break;
            }
            let adjusted = adjust_splits(splits, sender, receiver, step);
            if !visited.contains(&adjusted) {
                return Some((adjusted, receiver));
            }
        }
    }
    None
}

/// Moves one step of flow from `sender` to `receiver` (`equal_start_v2.py:324-332`).
///
/// The step is capped at what the sender holds, so no split ever goes negative.
fn adjust_splits(
    splits: &[Fraction],
    sender: usize,
    receiver: usize,
    step: &BigRational,
) -> Vec<Fraction> {
    let delta = step
        .min(splits[sender].as_ratio())
        .clone();
    let mut adjusted = splits.to_vec();
    adjusted[receiver] = Fraction::new(adjusted[receiver].as_ratio() + &delta);
    adjusted[sender] = Fraction::new(adjusted[sender].as_ratio() - &delta);
    adjusted
}

/// How far an alternative fell short of its allocation, as a negative number
/// (`equal_start_v2.py:276`).
///
/// This is an on-chain amount ranked against human-unit prices, which is defibot's own mixing of
/// units. It is sound only because the value is negative and prices are not: whatever the scale,
/// a shortfall sorts below every price and lands at the worst end.
fn shortfall(sold: &BigUint, allocation: &BigUint) -> Option<f64> {
    let difference = BigInt::from(sold.clone()) - BigInt::from(allocation.clone());
    difference.to_f64()
}

/// Executed price of a sell whose proceeds are already net of gas (`equal_start.py:305-315`).
///
/// `None` when nothing was sold, which is how both ranking helpers spell "not comparable".
fn net_executed_price(
    sold: &BigUint,
    bought: &BigInt,
    sell_token: &Token,
    buy_token: &Token,
) -> Option<f64> {
    if sold.is_zero() {
        return None;
    }
    let sold = to_human_signed(&BigInt::from(sold.clone()), sell_token.decimals);
    let bought = to_human_signed(bought, buy_token.decimals);
    if sold == 0.0 {
        return None;
    }
    Some(bought / sold)
}

/// A signed on-chain amount in human units.
fn to_human_signed(amount: &BigInt, decimals: u32) -> f64 {
    let scaled = BigRational::new(amount.clone(), BigInt::from(10u8).pow(decimals));
    scaled.to_f64().unwrap_or(0.0)
}

/// Indices that sort `values` ascending, dropping the entries with no value
/// (`equal_start.py:391-428`).
///
/// The sort is stable, so equal values keep the caller's ordering. defibot's `np.argsort` defaults
/// to an unstable quicksort, which leaves ties resolved by whatever the partition happened to do;
/// stability is what makes a solve reproducible here.
fn argsort_ascending(values: &[Option<f64>]) -> Vec<usize> {
    argsort_ascending_masked(values, &vec![false; values.len()])
}

/// [`argsort_ascending`], additionally dropping every index marked in `ignore`.
fn argsort_ascending_masked(values: &[Option<f64>], ignore: &[bool]) -> Vec<usize> {
    let mut ordered: Vec<(usize, f64)> = values
        .iter()
        .enumerate()
        .filter(|(index, _)| !ignore[*index])
        // A NaN is dropped like a missing value: defibot's `np.argsort` sorts NaNs to the end and
        // then truncates exactly that many entries (`equal_start.py:425-427`).
        .filter_map(|(index, value)| {
            value
                .filter(|value| !value.is_nan())
                .map(|v| (index, v))
        })
        .collect();
    ordered.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ordered
        .into_iter()
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

    use super::*;
    use crate::{
        algorithm::{
            decomposition::{
                components::{Hop, PoolRef, SellLimitKind, SequentialRoute},
                optimizers::HopPool,
            },
            test_utils::{token, ConstantProductSim},
        },
        derived::types::TokenGasPrices,
    };

    /// `10^18`, the on-chain unit of the 18-decimal test tokens.
    fn unit() -> BigUint {
        BigUint::from(10u8).pow(18)
    }

    fn whole(amount: u64) -> BigUint {
        BigUint::from(amount) * unit()
    }

    fn pool(id: &str, reserve_a: u64, reserve_b: u64) -> PoolRef {
        PoolRef::new(
            id.to_string(),
            SellLimitKind::Enforced,
            Box::new(ConstantProductSim {
                reserve_0: whole(reserve_a),
                reserve_1: whole(reserve_b),
                gas: 100_000,
            }),
            None,
        )
    }

    /// A one-hop A -> B route over a single pool, with the hop's split already set.
    fn route(id: &str, reserve_a: u64, reserve_b: u64) -> SequentialRoute {
        let (token_a, token_b) = (token(0x0A, "A"), token(0x0B, "B"));
        let mut hop =
            Hop::new(token_a.clone(), token_b.clone(), vec![pool(id, reserve_a, reserve_b)])
                .expect("hop has a pool");
        hop.set_splits(vec![Fraction::one()])
            .expect("one split for one pool");
        SequentialRoute::new(vec![token_a, token_b], vec![hop]).expect("route matches its path")
    }

    fn free_gas(gas_price_wei: &BigUint) -> GasPrices {
        GasPrices::new(gas_price_wei.clone(), None)
    }

    fn optimize<S: Sellable>(
        routes: &mut [S],
        sell_amount: &BigUint,
        gas_prices: &GasPrices,
    ) -> SplitSolution {
        EqualStartV2::new(RankingMetric::MarginalPrice)
            .optimize(routes, sell_amount, gas_prices)
            .expect("equal start succeeds on well-formed pools")
    }

    fn splits_sum(solution: &SplitSolution) -> BigRational {
        solution
            .splits
            .iter()
            .fold(BigRational::zero(), |total, split| total + split.as_ratio())
    }

    fn ratio(numerator: i64, denominator: i64) -> Fraction {
        Fraction::from_ratio(numerator, denominator).expect("non-zero denominator")
    }

    fn half() -> Fraction {
        ratio(1, 2)
    }

    /// An evaluation that lets every alternative participate and carries `ranking` as both prices.
    fn evaluation(ranking: Vec<Option<f64>>) -> Evaluation {
        Evaluation {
            total_net: BigInt::zero(),
            executed_prices: ranking.clone(),
            marginal_prices: ranking.clone(),
            usable: vec![true; ranking.len()],
        }
    }

    #[test]
    fn test_two_equal_pools_split_evenly() {
        // Two identical constant-product pools are symmetric, so the optimum is exactly half each
        // — the equal start is already the answer and every move away from it loses.
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert_eq!(solution.splits, vec![half(), half()]);
        assert_eq!(solution.sold, sell_amount);
        assert_eq!(routes[0].sell_amount(), routes[1].sell_amount());
    }

    #[test]
    fn test_two_equal_pools_beat_a_single_pool() {
        let mut split_routes =
            vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let mut single_route = vec![route("a", 1_000_000, 1_000_000)];
        let sell_amount = whole(100_000);
        let gas_price_wei = BigUint::zero();

        let split = optimize(&mut split_routes, &sell_amount, &free_gas(&gas_price_wei));
        let single = optimize(&mut single_route, &sell_amount, &free_gas(&gas_price_wei));

        assert!(split.bought > single.bought);
    }

    #[test]
    fn test_unequal_pools_favour_the_deeper_one() {
        let mut routes = vec![route("shallow", 100_000, 100_000), route("deep", 900_000, 900_000)];
        let sell_amount = whole(50_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert!(
            solution.splits[1] > solution.splits[0],
            "expected the deeper pool to take more: {:?}",
            solution.splits
        );
        assert!(splits_sum(&solution) <= BigRational::one());
    }

    #[test]
    fn test_splits_sum_below_one_when_liquidity_is_short() {
        // ConstantProductSim caps a sell at half the input reserve, so neither pool can take more
        // than 5 whole tokens and the shortfall has to survive into the splits.
        let mut routes = vec![route("a", 10, 10), route("b", 10, 10)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert!(
            splits_sum(&solution) < BigRational::one(),
            "expected a shortfall, got {:?}",
            solution.splits
        );
        assert!(solution.sold < sell_amount);
        assert!(!solution.bought.is_zero());
    }

    #[test]
    fn test_rounding_never_sells_more_than_the_order() {
        // An order with no exact thirds: the per-alternative amounts are floored and clamped to
        // what is left, so the total can fall a wei short but can never overshoot.
        let mut routes = vec![
            route("a", 1_000_000, 1_000_000),
            route("b", 1_000_000, 1_000_000),
            route("c", 1_000_000, 1_000_000),
        ];
        let sell_amount = whole(999) + BigUint::from(7u8);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        let per_route: BigUint = routes
            .iter()
            .map(|route| route.sell_amount().clone())
            .sum();
        assert_eq!(solution.sold, per_route);
        assert!(solution.sold <= sell_amount);
        assert!(&sell_amount - &solution.sold < BigUint::from(routes.len()));
    }

    #[test]
    fn test_no_routes() {
        let mut routes: Vec<SequentialRoute> = Vec::new();
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &whole(1_000), &free_gas(&gas_price_wei));

        assert!(solution.splits.is_empty());
        assert!(solution.sold.is_zero());
    }

    #[test]
    fn test_zero_sell_amount_is_rejected() {
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let gas_price_wei = BigUint::zero();

        let error = EqualStartV2::new(RankingMetric::MarginalPrice)
            .optimize(&mut routes, &BigUint::zero(), &free_gas(&gas_price_wei))
            .expect_err("a zero sell amount has no splits");

        assert!(matches!(error, DecompositionError::InvalidStructure { .. }));
    }

    #[test]
    fn test_optimizer_splits_a_hop_over_its_pools() {
        // The same optimizer, one level down: the alternatives are a hop's pools, not branches.
        let (token_a, token_b) = (token(0x0A, "A"), token(0x0B, "B"));
        let mut hop = Hop::new(
            token_a,
            token_b,
            vec![pool("a", 1_000_000, 1_000_000), pool("b", 1_000_000, 1_000_000)],
        )
        .expect("hop has pools");
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = {
            let mut legs = HopPool::bind_all(&mut hop);
            optimize(&mut legs, &sell_amount, &free_gas(&gas_price_wei))
        };

        assert_eq!(solution.splits, vec![half(), half()]);
        hop.set_splits(solution.splits)
            .expect("one split per pool");
        assert!(hop.solved());
    }

    #[test]
    fn test_executed_price_ranking_reaches_the_same_symmetric_optimum() {
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = EqualStartV2::new(RankingMetric::ExecutedPrice)
            .optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei))
            .expect("executed-price ranking succeeds");

        assert_eq!(solution.splits, vec![half(), half()]);
    }

    #[test]
    fn test_initial_splits_are_equal_and_sum_to_one() {
        let splits = initial_splits(3);

        assert_eq!(splits, vec![ratio(1, 3); 3]);
        let total = splits
            .iter()
            .fold(BigRational::zero(), |total, split| total + split.as_ratio());
        assert!(total.is_one());
    }

    #[test]
    fn test_effective_splits_report_what_was_sold() {
        // The second pool is promised half the order but caps at 5 whole tokens, so its effective
        // split is what it took, not what it was offered.
        let mut routes = vec![route("deep", 1_000_000, 1_000_000), route("tiny", 10, 10)];
        let sell_amount = whole(1_000);

        let pass = effective_splits(&mut routes, &sell_amount, &[half(), half()])
            .expect("a shortfall is not an error");

        assert_eq!(pass.splits[0], half());
        assert!(pass.splits[1] < half());
        assert!(pass.splits[1] > Fraction::zero());
        assert!(routes[1].sell_amount() <= &whole(5));
        assert_eq!(pass.splits[1], split_of(routes[1].sell_amount(), &sell_amount));
    }

    #[test]
    fn test_effective_splits_reset_a_zero_split_route() {
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let sell_amount = whole(1_000);
        decrease_until_sell(&mut routes[1], &whole(100)).expect("sells");

        let pass =
            effective_splits(&mut routes, &sell_amount, &[Fraction::one(), Fraction::zero()])
                .expect("zeroing a route is not an error");

        assert_eq!(pass.splits, vec![Fraction::one(), Fraction::zero()]);
        assert!(routes[1].sell_amount().is_zero());
        assert!(routes[1].buy_amount().is_zero());
    }

    #[test]
    fn test_a_route_that_could_not_absorb_its_allocation_ranks_worst() {
        // The tiny pool sold 5 of the 500 it was promised. The shortfall has to sort it below the
        // deep pool even though the ranking price it was given is the better of the two.
        let mut routes = vec![route("deep", 1_000_000, 1_000_000), route("tiny", 10, 10)];
        let sell_amount = whole(1_000);
        let splits = vec![half(), half()];
        for (index, route) in routes.iter_mut().enumerate() {
            decrease_until_sell(route, &splits[index].apply(&sell_amount)).expect("sells");
        }

        let ranking = vec![Some(1.0), Some(2.0)];
        let ordered = worst_route_indexes(
            &routes,
            &splits,
            &sell_amount,
            &ranking,
            &evaluation(ranking.clone()),
        );

        assert_eq!(ordered, vec![1, 0]);
    }

    #[test]
    fn test_a_drained_route_is_never_drained_further() {
        let routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let splits = vec![Fraction::one(), Fraction::zero()];
        let ranking = vec![Some(2.0), Some(1.0)];

        let ordered = worst_route_indexes(
            &routes,
            &splits,
            &whole(1_000),
            &ranking,
            &evaluation(ranking.clone()),
        );

        assert_eq!(ordered, vec![0]);
    }

    #[test]
    fn test_a_full_route_cannot_receive() {
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let splits = vec![Fraction::one(), Fraction::zero()];
        // The full route has to have absorbed its allocation, or it is ranked as a shortfall
        // before the "already full" rule is ever reached (`equal_start_v2.py:298`, `:307`).
        decrease_until_sell(&mut routes[0], &whole(1_000)).expect("sells");
        let ranking = vec![Some(2.0), Some(1.0)];

        let ordered = best_route_indexes(
            &routes,
            &splits,
            &whole(1_000),
            &ranking,
            &evaluation(ranking.clone()),
            &[false, false],
        )
        .expect("ranking succeeds");

        assert_eq!(ordered, vec![1]);
    }

    #[test]
    fn test_a_saturated_route_cannot_receive() {
        let routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let splits = vec![half(), half()];
        let ranking = vec![Some(2.0), Some(1.0)];

        let ordered = best_route_indexes(
            &routes,
            &splits,
            &whole(1_000),
            &ranking,
            &evaluation(ranking.clone()),
            &[true, false],
        )
        .expect("ranking succeeds");

        assert_eq!(ordered, vec![1]);
    }

    #[test]
    fn test_an_unsold_route_falls_back_to_its_pre_trade_marginal_price() {
        // Neither route has been sold on, so both post-trade prices are missing. Without the
        // fallback both would be dropped and the search would have nowhere to move funds to.
        let routes = vec![route("cheap", 1_000_000, 500_000), route("rich", 1_000_000, 2_000_000)];
        let splits = vec![half(), half()];
        let ranking = vec![None, None];

        let ordered = best_route_indexes(
            &routes,
            &splits,
            &whole(1_000),
            &ranking,
            &evaluation(ranking.clone()),
            &[false, false],
        )
        .expect("ranking succeeds");

        assert_eq!(ordered, vec![1, 0]);
    }

    #[test]
    fn test_next_move_stops_when_every_option_is_visited() {
        let splits = vec![half(), half()];
        let step = BigRational::new(BigInt::one(), BigInt::from(2u8));
        let mut visited = BTreeSet::new();
        visited.insert(adjust_splits(&splits, 0, 1, &step));
        visited.insert(adjust_splits(&splits, 1, 0, &step));

        let next = next_move(&splits, &[0, 1], &[1, 0], &step, &visited);

        assert!(next.is_none(), "a fully visited neighbourhood must end the search");
    }

    #[test]
    fn test_next_move_takes_the_first_unvisited_option() {
        let splits = vec![half(), half()];
        let step = BigRational::new(BigInt::one(), BigInt::from(2u8));
        let visited = BTreeSet::new();

        let (adjusted, receiver) = next_move(&splits, &[0, 1], &[1, 0], &step, &visited)
            .expect("an unvisited move exists");

        assert_eq!(receiver, 1);
        assert_eq!(adjusted, vec![Fraction::zero(), Fraction::one()]);
    }

    #[test]
    fn test_a_move_never_takes_more_than_the_sender_holds() {
        let splits = vec![ratio(1, 10), half()];
        let step = BigRational::new(BigInt::one(), BigInt::from(2u8));

        let adjusted = adjust_splits(&splits, 0, 1, &step);

        assert_eq!(adjusted[0], Fraction::zero());
        assert_eq!(adjusted[1], ratio(3, 5));
    }

    #[test]
    fn test_argsort_drops_missing_and_masked_values() {
        let values = vec![None, Some(11.0), Some(12.0), Some(13.0)];

        assert_eq!(argsort_ascending(&values), vec![1, 2, 3]);
        assert_eq!(argsort_ascending_masked(&values, &[false, true, false, false]), vec![2, 3]);
        assert!(argsort_ascending(&[None, None]).is_empty());
    }

    #[test]
    fn test_the_saturation_rule_stops_a_pass_from_re_offering_a_losing_receiver() {
        // Four equal pools at the equal start, where every half-step away from it loses. Each
        // losing move saturates the route it was offered to, so the receiver list shrinks by one
        // per failure and the pass closes in nine evaluations. Without the rule the walk keeps
        // offering the same receivers from each remaining sender and takes thirteen.
        let optimizer = EqualStartV2::new(RankingMetric::MarginalPrice);
        let mut routes = (0..4)
            .map(|index| route(&format!("p{index}"), 1_000_000, 1_000_000))
            .collect::<Vec<_>>();
        let gas_price_wei = BigUint::zero();
        let start = initial_splits(routes.len());

        let pass = find_best_splits(
            &optimizer,
            &mut routes,
            &whole(100_000),
            &start,
            &free_gas(&gas_price_wei),
            &BigRational::new(BigInt::one(), BigInt::from(2u8)),
        )
        .expect("the pass converges");

        assert_eq!(pass.iterations, 9);
        assert_eq!(pass.splits, vec![ratio(1, 4); 4]);
    }

    #[test]
    fn test_the_saturation_rule_changes_where_a_pass_converges() {
        // Four pools of increasing depth, refined at a tenth of the order per move. Here the rule
        // is not just a shortcut: it steers the walk away from a receiver that already failed, and
        // the pass lands on a different — better — allocation than it would without it.
        let optimizer = EqualStartV2::new(RankingMetric::MarginalPrice);
        let reserves = [10_000u64, 40_000, 90_000, 160_000];
        let mut routes = reserves
            .iter()
            .enumerate()
            .map(|(index, reserve)| route(&format!("p{index}"), *reserve, *reserve))
            .collect::<Vec<_>>();
        let gas_price_wei = BigUint::zero();
        let start = initial_splits(routes.len());

        let pass = find_best_splits(
            &optimizer,
            &mut routes,
            &whole(100_000),
            &start,
            &free_gas(&gas_price_wei),
            &BigRational::new(BigInt::one(), BigInt::from(10u8)),
        )
        .expect("the pass converges");

        assert_eq!(pass.splits, vec![ratio(1, 20), ratio(3, 20), ratio(7, 20), ratio(9, 20)]);
    }

    #[test]
    fn test_argsort_keeps_the_input_order_on_ties() {
        let values = vec![Some(10.0), Some(10.0), Some(10.0)];

        assert_eq!(argsort_ascending(&values), vec![0, 1, 2]);
    }

    #[test]
    fn test_a_second_pool_is_dropped_when_its_gas_costs_more_than_it_buys() {
        // Splitting one whole token over two million-token pools saves about 5e11 of price impact.
        // Pricing a 100_000-gas swap at 1e13 makes the second pool cost twenty times that, so the
        // search has to concentrate the order — which it can only do by ranking passes and moves
        // on bought *minus gas* rather than on gross bought.
        let buy_token = token(0x0B, "B");
        let mut token_prices: TokenGasPrices = FxHashMap::default();
        token_prices.insert(
            buy_token.address.clone(),
            Price::new(BigUint::from(10u8).pow(17), BigUint::from(10u8).pow(18)),
        );
        let gas_price_wei = BigUint::from(10u8).pow(9);
        let sell_amount = whole(1);
        let pools = || vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];

        let free = optimize(&mut pools(), &sell_amount, &free_gas(&BigUint::zero()));
        let mut charged_routes = pools();
        let charged = optimize(
            &mut charged_routes,
            &sell_amount,
            &GasPrices::new(gas_price_wei.clone(), Some(Arc::new(token_prices.clone()))),
        );

        assert_eq!(free.splits, vec![half(), half()]);
        assert_eq!(charged.splits, vec![Fraction::zero(), Fraction::one()]);
        assert!(
            charged.bought < free.bought,
            "the chosen splits have to buy less gross than the ones they beat"
        );
        assert!(charged_routes[0]
            .sell_amount()
            .is_zero());
    }

    #[test]
    fn test_the_iteration_budget_stops_the_search_at_the_equal_start() {
        // Unequal pools: the search normally walks away from the equal start. A budget of zero
        // leaves it no move to make, so every pass returns what it was seeded with.
        let mut routes = vec![route("shallow", 100_000, 100_000), route("deep", 900_000, 900_000)];
        let sell_amount = whole(50_000);
        let gas_price_wei = BigUint::zero();

        let solution = EqualStartV2::new(RankingMetric::MarginalPrice)
            .with_max_move_iterations(Some(0))
            .optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei))
            .expect("a spent budget is not an error");

        assert_eq!(solution.splits, vec![half(), half()]);
    }
}
