//! Split solving for the decomposition algorithm.
//!
//! Port of `DecompositionOrderSolver`'s solving core:
//! `recursive_solve_splits` (`order_solver.py:575-699`), the post-processing of `_solve`
//! (`:270-298`), `sell_with_coupled_paths` (`utils.py:18-47`), `_remove_loops` (`:855-894`) and
//! `_solve_without_splits` (`:810-853`).
//!
//! # Two functions, not a node dispatch
//!
//! defibot's `recursive_solve_splits` walks a `FractalRoute` tree and dispatches on the node type.
//! Here the shape is named, so the walk is two functions that call each other:
//! [`solve_sequence_route`] threads a chain hop by hop, and [`solve_parallel_route`] divides one
//! level between its alternatives — recursing back into [`solve_sequence_route`] when those
//! alternatives are themselves chains.
//!
//! There is no third function for the graph. The whole solution *is* a parallel level over its
//! branches, so [`solve_parallel_route`] solves it too, with the outer optimizer for its own split
//! and the inner one for everything below. It used to be a separate `solve_graph`, and the copy had
//! already drifted: it checked for a zero amount before the single-alternative case, where defibot
//! checks it after, so a zero-amount single-branch graph ended up with a split of zero where
//! defibot leaves one.
//!
//! The semantics are defibot's, including the part that looks like a mistake: every alternative is
//! solved as if it were going to receive the *whole* amount (`order_solver.py:673-677`). That is
//! what makes them comparable to the split optimizer, and [`solve_decomposition_graph`]'s second
//! pass (`order_solver.py:285-296`) re-solves each branch for the amount it actually gets.
//!
//! # Sharing within a branch and sharing across branches are different problems
//!
//! A grouped branch holds its shared hop **once** for every tail hanging off it, and
//! [`solve_sequence_route`] sells that hop a single time for the whole branch. That is what stops
//! the optimizer from allocating one pool's liquidity once per token path passing through it.
//!
//! It does nothing about a pool shared between two *different* branches, and it is not supposed to:
//! [`sell_with_coupled_paths`] handles that afterwards, by re-selling the branches in sequence
//! against each other's post-trade liquidity. The two mechanisms compose because they correct
//! opposite halves of the same problem — conflating them, by also re-selling a branch's head per
//! tail, would charge the price impact of one swap several times and under-quote.

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{One, Zero};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, warn};
use tycho_simulation::tycho_core::{models::Address, simulation::protocol_sim::ProtocolSim};

use crate::{
    algorithm::decomposition::{
        components::{
            DecompositionError, DecompositionGraph, ParallelRoute, SequenceRoute, SplitKind,
        },
        models::{Fraction, TokenPriceData},
        optimizers::{decrease_until_sell, split_of, SplitOptimizer, SplitOptimizerConfig},
    },
    types::ComponentId,
};

/// Share of a route's own sell limit the solver is willing to use, as `(numerator, denominator)`.
///
/// `order_solver.py:589-593`. Exhausting a route is almost never a good trade: the last units
/// before a pool's limit execute at the worst price that pool will ever offer, and a route filled
/// to its cap leaves the re-solve passes that follow nothing to work with. defibot hard-codes
/// `0.8` with no configuration knob and it is kept that way here — a tunable would be one more
/// degree of freedom with no evidence behind it.
const SELL_LIMIT_UTILISATION: (u8, u8) = (4, 5);

/// Restarts [`solve_sequence_route`] will attempt before giving up on finding a size that fits.
///
/// Both recovery paths shrink the sell amount strictly, so the loop terminates on its own; the
/// bound only stops a pathological route from decrementing by one unit for an astronomical number
/// of rounds. defibot has no equivalent — it recurses, and hits Python's recursion limit instead.
const MAX_SOLVE_RESTARTS: usize = 64;

// ===================== recursive_solve_splits =====================

/// Solves one branch, threading each hop's output into the next (`order_solver.py:599-649`).
///
/// Two failures are recoverable and both restart the whole branch at a smaller size: a simulation
/// that could not be evaluated restarts at half the amount (`:619-625`), and a size the route
/// refuses restarts just below the reported limit (`:626-647`). defibot restarts by recursing;
/// this is a loop because the halving path can iterate once per bit of the sell amount and each of
/// those would otherwise be a stack frame.
///
/// # Errors
///
/// Whatever the optimizer or the underlying sells raise, once the size is no longer the problem.
pub(crate) fn solve_sequence_route(
    route: &mut SequenceRoute,
    sell_amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    let mut requested = sell_amount.clone();

    for _ in 0..MAX_SOLVE_RESTARTS {
        let (limit, _) = route.sell_amount_limit()?;
        let amount = clamp_to_limit(&requested, &limit);

        let mut hop_amount = amount.clone();
        let mut restart = None;
        for hop_ix in 0..route.hops().len() {
            // Below the top level the two optimizers are the same: only the graph's own split
            // gets the outer one.
            solve_parallel_route(
                &mut route.hops_mut()[hop_ix],
                &hop_amount,
                optimizer,
                optimizer,
                gas_prices,
            )?;

            match route.hops_mut()[hop_ix].sell(&hop_amount) {
                Ok((bought, _)) => hop_amount = bought,
                Err(DecompositionError::SellAmountLimit { limit, token, .. }) => {
                    let next = limit_restart_amount(route, hop_ix, &token, &limit, &amount)?;
                    debug!(
                        hop = hop_ix,
                        %limit,
                        %next,
                        "sell amount limit reached; restarting the branch below the limit"
                    );
                    restart = Some(next);
                    break;
                }
                // Ticks exhausted, or pool math that could not be evaluated at this size. Half the
                // amount is the only thing defibot tries, and it always terminates: integer
                // halving reaches zero, and a sell of zero cannot fail.
                Err(DecompositionError::Simulation { component, source }) => {
                    debug!(%component, %source, "simulation failed; restarting the branch at half");
                    restart = Some(&amount / 2u8);
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        let Some(next) = restart else {
            route.sell(&amount)?;
            return Ok(());
        };
        // Both recovery paths are meant to shrink; clamping keeps that true even if a cast-back
        // price ever reports something larger, which is what makes the loop finite.
        requested = if next < amount { next } else { minus_one(&amount) };
    }

    // Out of restarts. Backing off geometrically converges in a bounded number of steps and leaves
    // the branch holding the largest size it could actually fill.
    warn!("branch exhausted its solve restarts; falling back to a geometric back-off");
    decrease_until_sell(&requested, |amount| route.sell(amount))?;
    Ok(())
}

/// Divides one level's amount between its alternatives (`order_solver.py:648-699`).
///
/// The `ParallelRoute` branch of `recursive_solve_splits`, and the only one — the whole solution is
/// a `ParallelRoute` over its branches, so this solves the graph as well as a hop. Like defibot's,
/// it does not care whether the alternatives are pools or chains. An alternative that is itself a
/// chain has to be solved before the search can rank it, and every one is solved for the *whole*
/// amount: the optimizer can only compare alternatives sized on equal terms.
///
/// `optimizer` splits *this* level; `inner` splits everything below it. The two differ only at the
/// top, where [`SplitOptimizerConfig`] lets the split over branches use a different search from the
/// splits inside them. defibot has one optimizer throughout, so the pair is a fynd addition.
///
/// # Errors
///
/// Whatever the optimizer or the underlying sells raise.
pub(crate) fn solve_parallel_route(
    parallel_route: &mut ParallelRoute,
    sell_amount: &BigUint,
    optimizer: SplitOptimizer,
    inner: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    let (limit, _) = parallel_route.sell_amount_limit()?;
    let amount = clamp_to_limit(sell_amount, &limit);
    debug!(
        asked = %sell_amount,
        limit = %limit,
        searched_at = %amount,
        clamped = amount < *sell_amount,
        alternatives = parallel_route.inner().len(),
        "parallel level solve size after the sell-limit clamp"
    );

    // A single alternative carries everything, so there is no split to search (`:661-671`).
    if parallel_route.inner().len() == 1 {
        parallel_route.set_splits(vec![Fraction::one()])?;
        solve_one_alternative(&mut parallel_route.inner_mut()[0], &amount, inner, gas_prices)?;
        decrease_until_sell(&amount, |amount| parallel_route.sell(amount))?;
        return Ok(());
    }

    if !parallel_route
        .inner()
        .iter()
        .all(SplitKind::solved)
    {
        for child in parallel_route.inner_mut() {
            solve_one_alternative(child, &amount, inner, gas_prices)?;
        }
    }

    if amount.is_zero() {
        let zeros = vec![Fraction::zero(); parallel_route.inner().len()];
        parallel_route.set_splits(zeros)?;
        parallel_route.sell(&BigUint::zero())?;
        return Ok(());
    }

    let solution = optimizer.split(parallel_route.inner_mut(), &amount, gas_prices)?;
    for (index, split) in solution.splits.iter().enumerate() {
        let alternative = &parallel_route.inner()[index];
        debug!(
            alternative = %alternative.token_path_label(),
            split = split.to_f64(),
            settled = %alternative.sell_amount(),
            bought = %alternative.buy_amount(),
            asked = %amount,
            "split assigned to one alternative"
        );
    }
    parallel_route.set_splits(solution.splits)?;

    // An alternative the search settled on at zero keeps the amounts of its last trial sell
    // (`:694-696`).
    for index in zero_split_positions(parallel_route.splits()) {
        parallel_route.inner_mut()[index].sell(&BigUint::zero())?;
    }

    parallel_route.sell(&amount)?;
    Ok(())
}

/// `recursive_solve_splits` on one alternative (`order_solver.py:596-597`).
///
/// A chain has splits below it. A pool is defibot's `SimpleRoute`, which returns immediately
/// because there is nothing under it to solve.
fn solve_one_alternative(
    child: &mut SplitKind,
    amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    match child {
        SplitKind::Direct(_) => Ok(()),
        SplitKind::Sequence(chain) => solve_sequence_route(chain, amount, optimizer, gas_prices),
    }
}

/// The sell amount capped at [`SELL_LIMIT_UTILISATION`] of `limit` (`order_solver.py:589-593`).
fn clamp_to_limit(sell_amount: &BigUint, limit: &BigUint) -> BigUint {
    if sell_amount <= limit {
        return sell_amount.clone();
    }
    let (numerator, denominator) = SELL_LIMIT_UTILISATION;
    limit * BigUint::from(numerator) / BigUint::from(denominator)
}

/// Positions of the zero entries of a split vector.
fn zero_split_positions(splits: &[Fraction]) -> Vec<usize> {
    splits
        .iter()
        .enumerate()
        .filter(|(_, split)| split.is_zero())
        .map(|(index, _)| index)
        .collect()
}

/// Sell amount to restart a branch with after a hop refused its size (`order_solver.py:626-647`).
///
/// One unit below the reported limit, cast back into sell-token units when the limit was hit at an
/// intermediate token, and additionally capped at one unit below the amount just attempted. The cap
/// is what guarantees progress: the cast back through spot prices ignores the price impact of the
/// hops before it and can therefore land above the size that just failed.
fn limit_restart_amount(
    route: &SequenceRoute,
    hop_index: usize,
    limit_token: &Address,
    limit: &BigUint,
    sell_amount: &BigUint,
) -> Result<BigUint, DecompositionError> {
    let below_limit = minus_one(limit);
    if limit_token == &route.sell_token().address {
        return Ok(below_limit);
    }
    let cast = route.cast_to_sell_token(hop_index, &below_limit)?;
    Ok(cast.min(minus_one(sell_amount)))
}

// ===================== _solve post-processing =====================

/// Solves a candidate graph end to end (`order_solver.py:250-298`, minus the graph construction).
///
/// In order: solve the splits, drop branches containing a loop and re-solve if any were dropped,
/// re-solve each branch for the amount it was actually allocated, and finally re-sell the branches
/// against each other's post-trade liquidity.
///
/// Returns what the coupled-path sell realised: the bought amount and the gas.
///
/// # Errors
///
/// Whatever the optimizer or the underlying sells raise, and
/// [`DecompositionError::InvalidStructure`] when every branch turns out to contain a loop.
pub(crate) fn solve_decomposition_graph(
    graph: &mut DecompositionGraph,
    sell_amount: &BigUint,
    optimizers: SplitOptimizerConfig,
    gas_prices: &TokenPriceData,
) -> Result<(BigUint, BigUint), DecompositionError> {
    solve_parallel_route(graph, sell_amount, optimizers.outer, optimizers.inner, gas_prices)?;

    if remove_loops(graph)? {
        solve_parallel_route(graph, sell_amount, optimizers.outer, optimizers.inner, gas_prices)?;
    }

    // Every branch was solved as if it would receive the whole order. Now that the outer splits say
    // otherwise, each one is solved again for what it actually gets (`:285-296`).
    for index in 0..graph.inner().len() {
        let split = graph.splits()[index].clone();
        if split.is_zero() {
            continue;
        }
        let allocated = split.apply(sell_amount);
        graph.inner_mut()[index].reset_splits();
        solve_one_alternative(
            &mut graph.inner_mut()[index],
            &allocated,
            optimizers.inner,
            gas_prices,
        )?;
    }

    sell_with_coupled_paths(graph, sell_amount)
}

// ===================== _remove_loops =====================

/// Drops branches that trade a token pair in the direction another branch already claimed
/// (`order_solver.py:855-894`).
///
/// The registry is built across branches without being reset, so the first branch to claim a
/// direction wins and later branches trading it backwards are dropped.
///
/// Returns whether anything was removed; the caller must re-solve if so, because the outer splits
/// are cleared.
///
/// # Errors
///
/// [`DecompositionError::InvalidStructure`] when every branch contains a loop. defibot assigns the
/// empty branch list and produces a graph that raises on its next use; failing here lets the caller
/// fall back to the reference route instead.
pub(crate) fn remove_loops(graph: &mut DecompositionGraph) -> Result<bool, DecompositionError> {
    let mut registered: FxHashSet<(Address, Address)> = FxHashSet::default();
    let mut keep = vec![true; graph.inner().len()];
    let mut removed = false;

    for (index, branch) in graph.inner().iter().enumerate() {
        for hop in branch.all_hops() {
            // defibot reads directions off the executed swaps (`utils.py:168-190`), which exist
            // only where something was actually sold. A hop that carried nothing therefore neither
            // claims a direction nor trips over one.
            if !hop
                .pools()
                .iter()
                .any(|pool| !pool.sell_amount().is_zero())
            {
                continue;
            }

            let direction = (hop.sell_token().address.clone(), hop.buy_token().address.clone());
            let inverse = (direction.1.clone(), direction.0.clone());
            if registered.contains(&inverse) {
                keep[index] = false;
                removed = true;
            } else {
                registered.insert(direction);
            }
        }
    }

    if !removed {
        return Ok(false);
    }

    // This is not optimal: a whole top-level split is dropped because one pool in it forms a loop
    // with another branch. We are sacrificing the optimal solution for a simpler one to keep this
    // logic robust.
    graph.retain(&keep)?;
    Ok(true)
}

// ===================== sell_with_coupled_paths =====================

/// Re-sells the branches one at a time against each other's post-trade liquidity
/// (`utils.py:18-47`).
///
/// The optimizers score every branch against untouched market state, so two branches sharing a pool
/// each assume they get all of its liquidity. This walks the branches in order, feeding each one's
/// post-swap pool states into the branches that have not been sold yet, and returns what the
/// solution would really realise.
///
/// The saved pre-trade states are restored **unconditionally**, failure included — defibot puts the
/// revert in a `finally` (`utils.py:46-47`) because a graph left holding depleted states would
/// misprice every later solve against the same market snapshot.
///
/// Returns the bought amount and the gas.
///
/// # Errors
///
/// Whatever the branch sells raise; the states are restored first either way.
pub(crate) fn sell_with_coupled_paths(
    graph: &mut DecompositionGraph,
    sell_amount: &BigUint,
) -> Result<(BigUint, BigUint), DecompositionError> {
    let revert_state = snapshot_pool_states(graph);
    let result = sell_branches_in_sequence(graph, sell_amount);
    restore_pool_states(graph, &revert_state);
    result
}

/// The body of [`sell_with_coupled_paths`], with no state handling of its own.
fn sell_branches_in_sequence(
    graph: &mut DecompositionGraph,
    sell_amount: &BigUint,
) -> Result<(BigUint, BigUint), DecompositionError> {
    if graph.splits().is_empty() {
        return Err(DecompositionError::Unsolved {
            token_in: graph.sell_token().address.clone(),
            token_out: graph.buy_token().address.clone(),
        });
    }

    let mut bought = BigUint::zero();
    let mut gas = BigUint::zero();
    for index in 0..graph.inner().len() {
        let split = graph.splits()[index].clone();
        // Logged for the zero case too: a branch the search dropped is as informative as one it
        // kept when the question is how the order was divided.
        debug!(
            branch = %graph.inner()[index].token_path_label(),
            split = split.to_f64(),
            hop_splits = ?graph.inner()[index]
                .all_hops()
                .iter()
                .map(|hop| hop.splits().iter().map(Fraction::to_f64).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "outer split over one branch"
        );
        if split.is_zero() {
            continue;
        }

        let amount = round_to_nearest(split.as_ratio(), sell_amount);
        let (branch_bought, branch_gas) =
            decrease_until_sell(&amount, |amount| graph.inner_mut()[index].sell(amount))?;
        debug!(
            branch = %graph.inner()[index].token_path_label(),
            requested = %amount,
            settled = %graph.inner()[index].sell_amount(),
            bought = %branch_bought,
            "branch sold against the liquidity earlier branches left"
        );
        bought += branch_bought;
        gas += branch_gas;

        propagate_pool_states(graph, index);
    }

    let sold = graph
        .inner()
        .iter()
        .fold(BigUint::zero(), |total, branch| total + branch.sell_amount());
    graph.record_sell(sold, bought.clone());
    Ok((bought, gas))
}

/// Pre-trade state of every pool in the graph, one entry per component.
///
/// Two branches holding the same component hold separate copies of its state; they start out equal,
/// so keeping the first is enough to restore both (`utils.py:28`).
fn snapshot_pool_states(
    graph: &DecompositionGraph,
) -> FxHashMap<ComponentId, Box<dyn ProtocolSim>> {
    let mut snapshot: FxHashMap<ComponentId, Box<dyn ProtocolSim>> = FxHashMap::default();
    for pool in graph_pools(graph) {
        snapshot
            .entry(pool.0.clone())
            .or_insert_with(|| pool.1.clone_box());
    }
    snapshot
}

/// Component id and pre-trade state of every pool in the graph, in branch order.
fn graph_pools(graph: &DecompositionGraph) -> Vec<(&ComponentId, &dyn ProtocolSim)> {
    graph
        .inner()
        .iter()
        .flat_map(SplitKind::all_hops)
        .flat_map(ParallelRoute::pools)
        .map(|pool| (pool.component_id(), pool.state()))
        .collect()
}

/// Writes `branch`'s post-trade pool states into that branch and every branch after it
/// (`utils.py:42`, `:50-60`).
fn propagate_pool_states(graph: &mut DecompositionGraph, branch: usize) {
    let updates: Vec<(ComponentId, Box<dyn ProtocolSim>)> = graph.inner()[branch]
        .all_hops()
        .into_iter()
        .flat_map(ParallelRoute::pools)
        .filter_map(|pool| {
            pool.new_state()
                .map(|state| (pool.component_id().clone(), state.clone_box()))
        })
        .collect();
    if updates.is_empty() {
        return;
    }

    // defibot updates from `inner_routes[i:]`, the sold branch included: a branch using the same
    // pool at two hops must see its own first hop's effect at the second.
    for branch in &mut graph.inner_mut()[branch..] {
        let mut changed = false;
        branch.for_each_pool_mut(&mut |pool| {
            let Some((_, saved)) = updates
                .iter()
                .find(|(component, _)| component == pool.component_id())
            else {
                return;
            };
            pool.update_state(saved.clone_box());
            changed = true;
        });
        if changed {
            branch.invalidate();
        }
    }
}

/// Restores the saved pre-trade states (`utils.py:63-71`).
///
/// Recorded sell and buy amounts survive: they are the result being computed.
fn restore_pool_states(
    graph: &mut DecompositionGraph,
    snapshot: &FxHashMap<ComponentId, Box<dyn ProtocolSim>>,
) {
    for branch in graph.inner_mut() {
        branch.for_each_pool_mut(&mut |pool| {
            let Some(state) = snapshot.get(pool.component_id()) else {
                return;
            };
            pool.update_state(state.clone_box());
        });
        branch.invalidate();
    }
}

/// `round(ratio * amount)`, breaking ties towards the even integer.
///
/// defibot applies Python's `round` here (`utils.py:38`) rather than the truncation every other
/// split application uses, and Python rounds halves to even. A negative ratio is meaningless for
/// routing and yields zero, matching [`Fraction::apply`].
fn round_to_nearest(ratio: &BigRational, amount: &BigUint) -> BigUint {
    let scaled = BigInt::from(amount.clone()) * ratio.numer();
    if scaled <= BigInt::zero() {
        return BigUint::zero();
    }

    let denominator = ratio.denom();
    let quotient = &scaled / denominator;
    let remainder = &scaled - &quotient * denominator;
    let doubled = remainder * 2;
    let round_up =
        doubled > *denominator || (doubled == *denominator && (&quotient % 2u8) == BigInt::one());

    let rounded = if round_up { quotient + BigInt::one() } else { quotient };
    rounded
        .to_biguint()
        .unwrap_or_else(BigUint::zero)
}

/// One less than `value`, floored at zero.
fn minus_one(value: &BigUint) -> BigUint {
    if value.is_zero() {
        BigUint::zero()
    } else {
        value - BigUint::one()
    }
}

// ===================== _solve_without_splits =====================

/// Sells whatever the branches can absorb when the normal solve bought nothing
/// (`order_solver.py:810-853`).
///
/// Each branch is backed off until it finds a size it can fill, the branches are ranked on what
/// they buy net of gas, and the sell amount is handed out greedily in that order. A branch reusing
/// a pool an already-included branch depends on is skipped rather than double-counted.
///
/// The resulting splits **deliberately need not sum to one**: if the market cannot absorb the whole
/// order, the shortfall is the answer and normalising it away would claim liquidity that is not
/// there.
///
/// # Deviations from defibot
///
/// defibot `break`s out of the allocation loop once the amount is exhausted (`:833-834`), leaving
/// every branch it never reached holding the size its earlier back-off found — and then builds the
/// splits from those sizes (`:847-849`). The splits then sum to more than one and the closing
/// `route.sell(order.sell_amount)` (`:852`) sells more than the order. Branches past the exhaustion
/// point are zeroed here instead.
///
/// # Errors
///
/// Whatever the branch sells raise.
pub(crate) fn solve_without_splits(
    graph: &mut DecompositionGraph,
    sell_amount: &BigUint,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    for branch in graph.inner_mut() {
        decrease_until_sell(sell_amount, |amount| branch.sell(amount))?;
    }

    let mut ranked: Vec<(usize, BigInt)> = graph
        .inner()
        .iter()
        .enumerate()
        .map(|(index, branch)| {
            let cost = gas_prices.cost_in_token(&branch.gas(), &branch.buy_token().address);
            (index, BigInt::from(branch.buy_amount().clone()) - BigInt::from(cost))
        })
        .collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1));

    let mut remaining = sell_amount.clone();
    let mut included: FxHashSet<ComponentId> = FxHashSet::default();
    for (index, _) in ranked {
        let used = traded_components(&graph.inner()[index]);
        let reuses_pool = used
            .iter()
            .any(|component| included.contains(component));

        if remaining.is_zero() || reuses_pool {
            graph.inner_mut()[index].sell(&BigUint::zero())?;
            continue;
        }

        included.extend(used);
        let allocated = remaining.clone().min(
            graph.inner()[index]
                .sell_amount()
                .clone(),
        );
        graph.inner_mut()[index].sell(&allocated)?;
        remaining -= allocated;
    }

    let splits = graph
        .inner()
        .iter()
        .map(|branch| split_of(branch.sell_amount(), sell_amount))
        .collect();
    graph.set_splits(splits)?;

    // defibot closes with `route.sell(order.sell_amount)` (`:852`), which re-runs every branch at
    // `floor(amount * split)` and first checks the amount against the graph's own limit. That check
    // rejects exactly the case this function exists for — an order larger than the market can
    // absorb — and the re-run can only move the per-branch amounts around by a rounding unit. The
    // branches already hold the sizes they proved they can fill, so the totals are recorded rather
    // than recomputed.
    let bought = graph
        .inner()
        .iter()
        .fold(BigUint::zero(), |total, branch| total + branch.buy_amount());
    graph.record_sell(sell_amount.clone(), bought);
    Ok(())
}

/// Components a branch actually traded on (`order_solver.py:836`).
fn traded_components(branch: &SplitKind) -> Vec<ComponentId> {
    branch
        .all_hops()
        .into_iter()
        .flat_map(ParallelRoute::pools)
        .filter(|pool| pool.new_state().is_some())
        .map(|pool| pool.component_id().clone())
        .collect()
}

// ===================== Final comparison =====================

/// Which of two solved graphs a caller should return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SolutionChoice {
    /// The graph built from the full candidate subgraph.
    Candidate,
    /// The small reference route, which the candidate failed to beat.
    Reference,
}

/// What a solved graph buys net of the gas it spends, in on-chain buy-token units
/// (`order_solver.py:300-302`).
///
/// Signed: a route can cost more gas than it buys.
pub(crate) fn net_of_gas(graph: &DecompositionGraph, gas_prices: &TokenPriceData) -> BigInt {
    let cost = gas_prices.cost_in_token(&graph.gas(), &graph.buy_token().address);
    BigInt::from(graph.buy_amount().clone()) - BigInt::from(cost)
}

/// Picks the better of the candidate and the reference route (`order_solver.py:304-310`).
///
/// The reference wins ties in the sense that the candidate must be *strictly* better to be chosen —
/// it is the deliberately small, safe route, and a candidate that only matches it is not worth its
/// extra complexity.
pub(crate) fn choose_solution(
    candidate: &DecompositionGraph,
    reference: Option<&DecompositionGraph>,
    gas_prices: &TokenPriceData,
) -> SolutionChoice {
    let Some(reference) = reference else {
        return SolutionChoice::Candidate;
    };
    if net_of_gas(candidate, gas_prices) < net_of_gas(reference, gas_prices) {
        SolutionChoice::Reference
    } else {
        SolutionChoice::Candidate
    }
}

#[cfg(test)]
#[path = "tests/solve_tests.rs"]
mod tests;
