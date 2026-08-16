//! Split solving for the decomposition algorithm.
//!
//! Port of `DecompositionOrderSolver`'s solving core:
//! `recursive_solve_splits` (`order_solver.py:575-699`), the post-processing of `_solve`
//! (`:270-298`), `sell_with_coupled_paths` (`utils.py:18-47`), `_remove_loops` (`:855-894`) and
//! `_solve_without_splits` (`:810-853`).
//!
//! # Passes, not a recursion
//!
//! defibot's `recursive_solve_splits` walks an arbitrarily deep `FractalRoute` tree and dispatches
//! on the node type. The fixed structure ([`DecompositionGraph`] / [`Branch`] / [`SequentialRoute`]
//! / [`Hop`]) turns that into explicit functions — [`solve_graph`], [`solve_branch`],
//! [`solve_route`] and [`solve_hop`] — that call each other in one direction only. The semantics
//! are unchanged, including the part that looks like a mistake: [`solve_graph`] solves every branch
//! as if it were going to receive the *whole* order (`order_solver.py:673-677`). That is what makes
//! the branches comparable to the split optimizer, and [`solve_solution_graph`]'s second pass
//! (`order_solver.py:285-296`) re-solves each branch for the amount it actually gets.
//!
//! # Sharing within a branch and sharing across branches are different problems
//!
//! A [`Branch`] holds its first hop **once** for every tail hanging off it, and [`solve_branch`]
//! sells that hop a single time for the whole branch. That is what stops the optimizer from
//! allocating one pool's liquidity once per token path leaving through it.
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
            Branch, BranchSide, DecompositionError, DecompositionGraph, Fraction, Hop,
            SequentialRoute,
        },
        models::TokenPriceData,
        optimizers::{
            decrease_until_sell, split_of, HopPool, Sellable, SplitOptimizer, SplitOptimizerConfig,
        },
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

/// Restarts [`solve_route`] will attempt before giving up on finding a size that fits.
///
/// Both recovery paths shrink the sell amount strictly, so the loop terminates on its own; the
/// bound only stops a pathological route from decrementing by one unit for an astronomical number
/// of rounds. defibot has no equivalent — it recurses, and hits Python's recursion limit instead.
const MAX_SOLVE_RESTARTS: usize = 64;

// ===================== recursive_solve_splits =====================

/// Solves the outer splits of a graph and everything below them (`order_solver.py:651-699`).
///
/// Every branch is first solved for the *whole* `sell_amount` so the optimizer can compare them on
/// equal terms; see the module docs for why that is not corrected here.
///
/// # Errors
///
/// Whatever the optimizer or the underlying sells raise. A recoverable failure is retried at a
/// smaller size rather than propagated.
pub(crate) fn solve_graph(
    graph: &mut DecompositionGraph,
    sell_amount: &BigUint,
    optimizers: SplitOptimizerConfig,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    let (limit, _) = graph.sell_amount_limit()?;
    let amount = clamp_to_limit(sell_amount, &limit);
    debug!(
        order = %sell_amount,
        graph_limit = %limit,
        searched_at = %amount,
        clamped = amount < *sell_amount,
        "graph solve size after the sell-limit clamp"
    );

    // A single branch carries everything, so there is no split to search (`:661-671`).
    if graph.branches().len() == 1 {
        graph.set_outer_splits(vec![Fraction::one()])?;
        solve_branch(&mut graph.branches_mut()[0], &amount, optimizers.inner, gas_prices)?;
        decrease_until_sell(graph, &amount)?;
        return Ok(());
    }

    if !graph
        .branches()
        .iter()
        .all(Branch::solved)
    {
        for branch in graph.branches_mut() {
            solve_branch(branch, &amount, optimizers.inner, gas_prices)?;
        }
    }

    if amount.is_zero() {
        let zeros = vec![Fraction::zero(); graph.branches().len()];
        graph.set_outer_splits(zeros)?;
        graph.sell(&BigUint::zero())?;
        return Ok(());
    }

    let solution = optimizers
        .outer
        .split(graph.branches_mut(), &amount, gas_prices)?;
    for (index, split) in solution.splits.iter().enumerate() {
        let branch = &graph.branches()[index];
        let tails = branch
            .sequences()
            .iter()
            .map(|tail| {
                tail.hops()
                    .iter()
                    .map(|hop| hop.token_out().symbol.as_str())
                    .collect::<Vec<_>>()
                    .join(">")
            })
            .collect::<Vec<_>>()
            .join(" | ");
        debug!(
            head = %format!("{}>{}", branch.hop().token_in().symbol, branch.hop().token_out().symbol),
            %tails,
            split = split.to_f64(),
            settled = %branch.sell_amount(),
            bought = %branch.buy_amount(),
            asked = %amount,
            "outer split assigned to a branch"
        );
    }
    graph.set_outer_splits(solution.splits)?;

    // A branch the search settled on at zero keeps the amounts of its last trial sell, which would
    // otherwise be read back as part of the solution (`:694-696`).
    for index in zero_split_positions(graph.outer_splits()) {
        graph.branches_mut()[index].sell(&BigUint::zero())?;
    }

    graph.sell(&amount)?;
    Ok(())
}

/// Solves one branch: the shared head's pool splits, then the split of its output over the tails.
///
/// A branch is `Sequential[head, Parallel[tails]]` (`order_solver.py:517-554`), so this is
/// `recursive_solve_splits` walking a two-leg sequence whose second leg is a parallel route. The
/// restart machinery is [`solve_route`]'s, applied to those two legs: a simulation that could not
/// be evaluated restarts at half the amount (`:619-625`) and a size the branch refuses restarts
/// just below the reported limit (`:626-647`).
///
/// The head is sold **once** here, for the branch's whole amount, and every tail is then sized
/// against that single sell's output. A branch with no tails is just its head.
///
/// # Errors
///
/// Whatever the optimizer or the underlying sells raise, once the size is no longer the problem.
pub(crate) fn solve_branch(
    branch: &mut Branch,
    sell_amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    let mut requested = sell_amount.clone();

    for _ in 0..MAX_SOLVE_RESTARTS {
        let (limit, _) = branch.sell_amount_limit()?;
        let amount = clamp_to_limit(&requested, &limit);

        let restart = match branch.side() {
            BranchSide::Head => solve_hop_then_sequences(branch, &amount, optimizer, gas_prices)?,
            BranchSide::Tail => solve_sequences_then_hop(branch, &amount, optimizer, gas_prices)?,
        };
        match restart {
            None => {
                branch.sell(&amount)?;
                return Ok(());
            }
            Some(next) => {
                requested = next;
                continue;
            }
        }
    }

    // Out of restarts. Backing off geometrically converges in a bounded number of steps and leaves
    // the branch holding the largest size it could actually fill.
    warn!("branch exhausted its solve restarts; falling back to a geometric back-off");
    decrease_until_sell(branch, &requested)?;
    Ok(())
}

/// One attempt at a [`BranchSide::Head`] branch: sell the hop, then split its output.
///
/// Returns `None` when the attempt succeeded, or `Some(amount)` to restart the branch at a smaller
/// size.
fn solve_hop_then_sequences(
    branch: &mut Branch,
    amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<Option<BigUint>, DecompositionError> {
    {
        let amount = amount;
        // Leg one: the branch's own hop. Its limit is already in the branch's sell token, so a
        // refusal needs no cast back.
        solve_hop(branch.hop_mut(), amount, optimizer, gas_prices)?;
        let hop_out = match branch.hop_mut().sell(amount) {
            Ok((bought, _)) => bought,
            Err(DecompositionError::SellAmountLimit { limit, .. }) => {
                return Ok(Some(shrink_below(&minus_one(&limit), amount)));
            }
            Err(DecompositionError::Simulation { component, source }) => {
                debug!(%component, %source, "hop simulation failed; restarting the branch at half");
                return Ok(Some(amount / 2u8));
            }
            Err(error) => return Err(error),
        };

        // Leg two: the parallel sequences, splitting what the hop actually produced.
        match solve_sequences(branch, &hop_out, optimizer, gas_prices) {
            Ok(()) => Ok(None),
            Err(DecompositionError::SellAmountLimit { limit, .. }) => {
                // The limit is denominated in the hop's output token; casting it back through the
                // hop's spot price ignores the hop's own impact and can land above the size that
                // just failed, so it is capped below it to keep the loop finite.
                let cast = branch.cast_from_hop_out(&minus_one(&limit))?;
                debug!(%limit, %cast, "sequence limit reached; restarting the branch below it");
                Ok(Some(shrink_below(&cast, amount)))
            }
            Err(DecompositionError::Simulation { component, source }) => {
                debug!(%component, %source, "sequence simulation failed; restarting at half");
                Ok(Some(amount / 2u8))
            }
            Err(error) => Err(error),
        }
    }
}

/// One attempt at a [`BranchSide::Tail`] branch: split across the sequences, then sell the hop
/// once with everything they produced.
///
/// The hop is sold a single time for the branch's whole flow, which is what stops sequences that
/// converge on one pool from each being priced as though they owned it.
///
/// Returns `None` when the attempt succeeded, or `Some(amount)` to restart at a smaller size.
fn solve_sequences_then_hop(
    branch: &mut Branch,
    amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<Option<BigUint>, DecompositionError> {
    // Leg one: the parallel sequences, splitting the branch's own amount. Their limits are already
    // in the branch's sell token, so a refusal needs no cast back.
    match solve_sequences(branch, amount, optimizer, gas_prices) {
        Ok(()) => {}
        Err(DecompositionError::SellAmountLimit { limit, .. }) => {
            return Ok(Some(shrink_below(&minus_one(&limit), amount)));
        }
        Err(DecompositionError::Simulation { component, source }) => {
            debug!(%component, %source, "sequence simulation failed; restarting at half");
            return Ok(Some(amount / 2u8));
        }
        Err(error) => return Err(error),
    }

    let into_hop: BigUint = branch
        .sequences()
        .iter()
        .map(SequentialRoute::buy_amount)
        .sum();

    // Leg two: the shared hop, taking everything the sequences delivered.
    solve_hop(branch.hop_mut(), &into_hop, optimizer, gas_prices)?;
    match branch.hop_mut().sell(&into_hop) {
        Ok(_) => Ok(None),
        Err(DecompositionError::SellAmountLimit { limit, .. }) => {
            // The limit is in the hop's input token, which is what the sequences deliver. Casting
            // it back through their combined price ignores their own impact and can land above the
            // size that just failed, so it is capped below it to keep the loop finite.
            let cast = branch.cast_to_sequence_in(&minus_one(&limit))?;
            debug!(%limit, %cast, "hop limit reached; restarting the branch below it");
            Ok(Some(shrink_below(&cast, amount)))
        }
        Err(DecompositionError::Simulation { component, source }) => {
            debug!(%component, %source, "hop simulation failed; restarting the branch at half");
            Ok(Some(amount / 2u8))
        }
        Err(error) => Err(error),
    }
}

/// Splits `amount` across a branch's sequences (`order_solver.py:651-699`).
///
/// Every tail is first solved for the *whole* head output, for the same reason [`solve_graph`]
/// solves every branch for the whole order: the optimizer can only compare alternatives that were
/// sized on equal terms.
///
/// # Errors
///
/// Whatever the optimizer or the tail sells raise, with limits denominated in the head's output
/// token so the caller can cast them back.
fn solve_sequences(
    branch: &mut Branch,
    amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    // A branch with no tails ends at its head; there is nothing below it to split.
    if branch.sequences().is_empty() {
        return branch.set_splits(Vec::new());
    }

    // A single tail takes everything the head produced (`:661-671`).
    if branch.sequences().len() == 1 {
        branch.set_splits(vec![Fraction::one()])?;
        return solve_route(&mut branch.sequences_mut()[0], amount, optimizer, gas_prices);
    }

    if !branch
        .sequences()
        .iter()
        .all(SequentialRoute::solved)
    {
        for tail in branch.sequences_mut() {
            solve_route(tail, amount, optimizer, gas_prices)?;
        }
    }

    if amount.is_zero() {
        let zeros = vec![Fraction::zero(); branch.sequences().len()];
        branch.set_splits(zeros)?;
        for tail in branch.sequences_mut() {
            tail.sell(&BigUint::zero())?;
        }
        return Ok(());
    }

    let solution = optimizer.split(branch.sequences_mut(), amount, gas_prices)?;
    branch.set_splits(solution.splits)?;

    // A tail the search settled on at zero keeps the amounts of its last trial sell (`:694-696`).
    for index in zero_split_positions(branch.splits()) {
        branch.sequences_mut()[index].sell(&BigUint::zero())?;
    }
    Ok(())
}

/// `candidate`, or one unit below `attempted` when the candidate would not shrink the request.
///
/// Every restart path must decrease strictly; that is what makes the restart loop finite.
fn shrink_below(candidate: &BigUint, attempted: &BigUint) -> BigUint {
    if candidate < attempted {
        candidate.clone()
    } else {
        minus_one(attempted)
    }
}

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
pub(crate) fn solve_route(
    route: &mut SequentialRoute,
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
        for index in 0..route.hops().len() {
            solve_hop(&mut route.hops_mut()[index], &hop_amount, optimizer, gas_prices)?;

            match route.hops_mut()[index].sell(&hop_amount) {
                Ok((bought, _)) => hop_amount = bought,
                Err(DecompositionError::SellAmountLimit { limit, token, .. }) => {
                    let next = limit_restart_amount(route, index, &token, &limit, &amount)?;
                    debug!(
                        hop = index,
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
    decrease_until_sell(route, &requested)?;
    Ok(())
}

/// Solves the pool splits of one hop (`order_solver.py:651-699`, at the level below the graph).
///
/// # Errors
///
/// Whatever the optimizer or the underlying pool sells raise.
pub(crate) fn solve_hop(
    hop: &mut Hop,
    sell_amount: &BigUint,
    optimizer: SplitOptimizer,
    gas_prices: &TokenPriceData,
) -> Result<(), DecompositionError> {
    let (limit, _) = hop.sell_amount_limit()?;
    let amount = clamp_to_limit(sell_amount, &limit);

    if hop.pools().len() == 1 {
        hop.set_splits(vec![Fraction::one()])?;
        decrease_until_sell(hop, &amount)?;
        return Ok(());
    }

    if amount.is_zero() {
        let zeros = vec![Fraction::zero(); hop.pools().len()];
        hop.set_splits(zeros)?;
        hop.sell(&BigUint::zero())?;
        return Ok(());
    }

    let solution = {
        let mut legs = HopPool::bind_all(hop);
        optimizer.split(&mut legs, &amount, gas_prices)?
    };
    hop.set_splits(solution.splits)?;

    let zeros = zero_split_positions(hop.splits());
    if !zeros.is_empty() {
        let mut legs = HopPool::bind_all(hop);
        for index in zeros {
            legs[index].sell(&BigUint::zero())?;
        }
    }

    hop.sell(&amount)?;
    Ok(())
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
    route: &SequentialRoute,
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
pub(crate) fn solve_solution_graph(
    graph: &mut DecompositionGraph,
    sell_amount: &BigUint,
    optimizers: SplitOptimizerConfig,
    gas_prices: &TokenPriceData,
) -> Result<(BigUint, BigUint), DecompositionError> {
    solve_graph(graph, sell_amount, optimizers, gas_prices)?;

    if remove_loops(graph)? {
        solve_graph(graph, sell_amount, optimizers, gas_prices)?;
    }

    // Every branch was solved as if it would receive the whole order. Now that the outer splits say
    // otherwise, each one is solved again for what it actually gets (`:285-296`).
    for index in 0..graph.branches().len() {
        let split = graph.outer_splits()[index].clone();
        if split.is_zero() {
            continue;
        }
        let allocated = split.apply(sell_amount);
        reset_branch_splits(&mut graph.branches_mut()[index]);
        solve_branch(&mut graph.branches_mut()[index], &allocated, optimizers.inner, gas_prices)?;
    }

    sell_with_coupled_paths(graph, sell_amount)
}

/// Marks a branch and everything below it unsolved (`order_solver.py:714-720`).
///
/// The tail split goes too: it divided an output the head produced at the old size, and the branch
/// is about to be re-solved at a different one.
fn reset_branch_splits(branch: &mut Branch) {
    for hop in branch.hops_mut() {
        // The vector is empty, so the arity check cannot fail.
        let _ = hop.set_splits(Vec::new());
    }
    let _ = branch.set_splits(Vec::new());
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
    let mut keep = vec![true; graph.branches().len()];
    let mut removed = false;

    for (index, branch) in graph.branches().iter().enumerate() {
        for hop in branch.hops() {
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

            let direction = (hop.token_in().address.clone(), hop.token_out().address.clone());
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
    graph.retain_branches(&keep)?;
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
    if graph.outer_splits().is_empty() {
        return Err(DecompositionError::Unsolved {
            token_in: graph.sell_token().address.clone(),
            token_out: graph.buy_token().address.clone(),
        });
    }

    let mut bought = BigUint::zero();
    let mut gas = BigUint::zero();
    for index in 0..graph.branches().len() {
        let split = graph.outer_splits()[index].clone();
        // Logged for the zero case too: a branch the search dropped is as informative as one it
        // kept when the question is how the order was divided.
        debug!(
            branch = %graph.branches()[index].token_path_label(),
            split = split.to_f64(),
            tail_splits = ?graph.branches()[index]
                .splits()
                .iter()
                .map(Fraction::to_f64)
                .collect::<Vec<_>>(),
            "outer split over one branch"
        );
        if split.is_zero() {
            continue;
        }

        let amount = round_to_nearest(split.as_ratio(), sell_amount);
        let (branch_bought, branch_gas) =
            decrease_until_sell(&mut graph.branches_mut()[index], &amount)?;
        debug!(
            branch = %graph.branches()[index].token_path_label(),
            requested = %amount,
            settled = %graph.branches()[index].sell_amount(),
            bought = %branch_bought,
            "branch sold against the liquidity earlier branches left"
        );
        bought += branch_bought;
        gas += branch_gas;

        propagate_pool_states(graph, index);
    }

    let sold = graph
        .branches()
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
        .branches()
        .iter()
        .flat_map(Branch::hops)
        .flat_map(Hop::pools)
        .map(|pool| (pool.component_id(), pool.state()))
        .collect()
}

/// Writes `branch`'s post-trade pool states into that branch and every branch after it
/// (`utils.py:42`, `:50-60`).
fn propagate_pool_states(graph: &mut DecompositionGraph, branch: usize) {
    let updates: Vec<(ComponentId, Box<dyn ProtocolSim>)> = graph.branches()[branch]
        .hops()
        .flat_map(Hop::pools)
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
    for branch in &mut graph.branches_mut()[branch..] {
        let mut changed = false;
        for hop in branch.hops_mut() {
            for pool in hop.pools_mut() {
                let Some((_, state)) = updates
                    .iter()
                    .find(|(component, _)| component == pool.component_id())
                else {
                    continue;
                };
                pool.update_state(state.clone_box());
                changed = true;
            }
        }
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
    for branch in graph.branches_mut() {
        for hop in branch.hops_mut() {
            for pool in hop.pools_mut() {
                let Some(state) = snapshot.get(pool.component_id()) else {
                    continue;
                };
                pool.update_state(state.clone_box());
            }
        }
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
    for branch in graph.branches_mut() {
        decrease_until_sell(branch, sell_amount)?;
    }

    let mut ranked: Vec<(usize, BigInt)> = graph
        .branches()
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
        let used = traded_components(&graph.branches()[index]);
        let reuses_pool = used
            .iter()
            .any(|component| included.contains(component));

        if remaining.is_zero() || reuses_pool {
            graph.branches_mut()[index].sell(&BigUint::zero())?;
            continue;
        }

        included.extend(used);
        let allocated = remaining.clone().min(
            graph.branches()[index]
                .sell_amount()
                .clone(),
        );
        graph.branches_mut()[index].sell(&allocated)?;
        remaining -= allocated;
    }

    let splits = graph
        .branches()
        .iter()
        .map(|branch| split_of(branch.sell_amount(), sell_amount))
        .collect();
    graph.set_outer_splits(splits)?;

    // defibot closes with `route.sell(order.sell_amount)` (`:852`), which re-runs every branch at
    // `floor(amount * split)` and first checks the amount against the graph's own limit. That check
    // rejects exactly the case this function exists for — an order larger than the market can
    // absorb — and the re-run can only move the per-branch amounts around by a rounding unit. The
    // branches already hold the sizes they proved they can fill, so the totals are recorded rather
    // than recomputed.
    let bought = graph
        .branches()
        .iter()
        .fold(BigUint::zero(), |total, branch| total + branch.buy_amount());
    graph.record_sell(sell_amount.clone(), bought);
    Ok(())
}

/// Components a branch actually traded on (`order_solver.py:836`).
fn traded_components(branch: &Branch) -> Vec<ComponentId> {
    branch
        .hops()
        .flat_map(Hop::pools)
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
