//! Pairwise split search.
//!
//! Port of `defibot/solver/order_solver/decomposition/optimizers/pair_comparison.py`.
//!
//! The search never looks at more than two alternatives at once. It ranks them, throws away the
//! ones that provably cannot help, solves the best two as a one-dimensional line search, then
//! absorbs the remaining alternatives one at a time — each again as a two-alternative problem. That
//! keeps the cost linear in the number of alternatives where a joint optimisation would be
//! exponential.
//!
//! # Deviations from defibot
//!
//! * defibot restores the caller's ordering by keying a dict on `route_info()`
//!   (`pair_comparison.py:47`, `:102-105`), a formatted human-readable string. Two structurally
//!   distinct routes can render identically and then silently receive each other's splits. This
//!   port carries indices into the caller's slice instead, so the mapping is exact by construction.
//! * Amounts are on-chain integers throughout. defibot mixes them with human-unit `Decimal`s and
//!   converts at the boundaries; the only place human units appear here is the net-price comparison
//!   in the pruning step, where a price is inherently a human-unit quantity.
//! * The pruning bound charges `minimum_gas` as defibot does, but computes it as the *realised* gas
//!   of the pools a route's splits activate. defibot's per-pool `minimum_swap_gas`
//!   (`routes/simple.py:256-257`) has no equivalent in Fynd's `ProtocolSim`. See
//!   [`Sellable::minimum_gas`] for why the activated-pools filter is the half that matters and why
//!   the remaining discrepancy is safe in the direction it errs.

use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};
use tracing::debug;
use tycho_simulation::tycho_core::models::token::Token;

use crate::algorithm::decomposition::{
    components::DecompositionError,
    models::{Fraction, TokenPriceData},
    optimizers::{decrease_until_sell, scale, split_of, to_human, Sellable, SplitSolution},
};

/// Fractions of the sell amount the line search moves per iteration, coarsest first.
///
/// `pair_comparison.py:138-144`. Each pass restarts from the previous pass's optimum, so the
/// schedule is a successive refinement: 1/2 finds the neighbourhood, 1/500 finds the point.
const STEPS: [(i64, i64); 5] = [(1, 2), (1, 5), (1, 10), (1, 50), (1, 500)];

/// [`DecompositionError::InvalidStructure`] when `sell_amount` is zero — every split in the search
/// is a ratio over it, and defibot raises `ZeroDivisionError` on the same input. Any
/// non-recoverable failure raised while selling is propagated.
pub(crate) fn split_by_pair_comparison<S: Sellable>(
    routes: &mut [S],
    sell_amount: &BigUint,
    gas_prices: &TokenPriceData,
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
        // Note that the split stays at one even when the back-off sold less than asked, so a
        // single-alternative shortfall shows up only in `sold` (`pair_comparison.py:39-42`).
        let (bought, _) = decrease_until_sell(sell_amount, |amount| routes[0].sell(amount))?;
        return Ok(SplitSolution {
            sold: routes[0].sell_amount().clone(),
            bought,
            splits: vec![Fraction::one()],
        });
    }

    let sell_token = routes[0].sell_token().clone();
    let buy_token = routes[0].buy_token().clone();
    let ranked = sort_routes(routes, sell_amount, gas_prices, &buy_token)?;

    let (to_analyse, skipped) =
        prune(routes, &ranked, sell_amount, gas_prices, &sell_token, &buy_token)?;
    debug_assert_eq!(to_analyse.len() + skipped.len(), routes.len());

    if to_analyse.len() == 1 {
        let index = to_analyse[0];
        let mut splits = vec![Fraction::zero(); routes.len()];
        splits[index] = split_of(routes[index].sell_amount(), sell_amount);
        let (bought, _) = decrease_until_sell(sell_amount, |amount| routes[index].sell(amount))?;
        return Ok(SplitSolution { sold: routes[index].sell_amount().clone(), bought, splits });
    }

    let (sold, bought, analysed_splits) =
        loop_through_pairs(routes, &to_analyse, sell_amount, gas_prices)?;

    // Skipped alternatives were reset to a zero sell, so their splits stay zero.
    let mut splits = vec![Fraction::zero(); routes.len()];
    for (position, &index) in to_analyse.iter().enumerate() {
        splits[index] = analysed_splits[position].clone();
    }

    Ok(SplitSolution { sold, bought, splits })
}

/// Ranks alternatives by what they buy net of gas when each is given the whole amount
/// (`pair_comparison.py:109-132`).
///
/// Returns `(index, score)` pairs, best first. The sort is stable, so equally scored alternatives
/// keep the caller's ordering.
fn sort_routes<S: Sellable>(
    routes: &mut [S],
    sell_amount: &BigUint,
    gas_prices: &TokenPriceData,
    buy_token: &Token,
) -> Result<Vec<(usize, BigInt)>, DecompositionError> {
    let mut ranked = Vec::with_capacity(routes.len());
    for (index, route) in routes.iter_mut().enumerate() {
        if !route.solved() {
            ranked.push((index, BigInt::zero()));
            continue;
        }
        let (bought, gas) = decrease_until_sell(sell_amount, |amount| route.sell(amount))?;
        let cost = gas_prices.cost_in_token(&gas, &buy_token.address);
        // Floored at zero so a route that cannot pay for its own gas ranks alongside one that
        // cannot trade at all; both are dropped by the caller.
        let net = (BigInt::from(bought) - BigInt::from(cost)).max(BigInt::zero());
        ranked.push((index, net));
    }
    ranked.sort_by(|left, right| right.1.cmp(&left.1));
    Ok(ranked)
}

/// Drops alternatives that provably cannot improve on the incumbent (`pair_comparison.py:51-81`).
///
/// The incumbent is the best-ranked alternative, and its realised net price is a price the solution
/// already achieves. Every other alternative is then judged on an *optimistic* net price built from
/// its zero-impact `marginal_price` — the price it would get if it could trade the whole amount
/// without moving. If even that is below what the incumbent already realises, no split can make the
/// alternative worth using and it is zeroed without ever being searched.
///
/// The bound holds only while a route's price decreases monotonically with size, which is true of
/// every pool Fynd simulates. It is what keeps the search linear on a market with hundreds of
/// candidate routes, so it is worth the assumption.
///
/// Returns the indices to search and the indices dropped.
///
/// # Invariant
///
/// Every term on the candidate side must stay optimistic. See [`Sellable::minimum_gas`] for why the
/// gas charge in particular is not free to adjust.
fn prune<S: Sellable>(
    routes: &mut [S],
    ranked: &[(usize, BigInt)],
    sell_amount: &BigUint,
    gas_prices: &TokenPriceData,
    sell_token: &Token,
    buy_token: &Token,
) -> Result<(Vec<usize>, Vec<usize>), DecompositionError> {
    let incumbent = ranked[0].0;
    let human_sell_amount = to_human(sell_amount, sell_token.decimals);
    // `minimum_gas`, not a sum over every pool: charging a candidate for pools its splits do not
    // activate pushes its optimistic price below the truth, and a bound that is not an upper bound
    // drops routes that would have improved the solution — see `Sellable::minimum_gas`. defibot
    // charges the same quantity at `pair_comparison.py:54` and `:72`.
    let gas_per_unit_sold = |route: &S| -> f64 {
        if human_sell_amount == 0.0 {
            return 0.0;
        }
        let cost = gas_prices.cost_in_token(&route.minimum_gas(), &buy_token.address);
        to_human(&cost, buy_token.decimals) / human_sell_amount
    };

    let best_net_price = routes[incumbent].executed_price() - gas_per_unit_sold(&routes[incumbent]);
    debug!(
        incumbent,
        best_net_price,
        incumbent_executed = routes[incumbent].executed_price(),
        incumbent_sold = %routes[incumbent].sell_amount(),
        order = %sell_amount,
        "pruning against the best-ranked alternative"
    );

    let mut to_analyse = vec![incumbent];
    let mut skipped = Vec::new();
    for (index, score) in &ranked[1..] {
        let index = *index;
        let optimistic_net_price = if score.is_zero() {
            f64::NEG_INFINITY
        } else {
            routes[index].marginal_price()? - gas_per_unit_sold(&routes[index])
        };
        debug!(
            index,
            optimistic_net_price,
            marginal_price = routes[index].marginal_price().unwrap_or(f64::NAN),
            sold = %routes[index].sell_amount(),
            kept = optimistic_net_price >= best_net_price,
            "pruning candidate"
        );

        if optimistic_net_price < best_net_price {
            // defibot calls `sell(0)` unconditionally here and raises on a route whose splits were
            // never set (`routes/parallel.py:179-180`); an unsolved route has nothing to reset.
            if routes[index].solved() {
                routes[index].sell(&BigUint::zero())?;
            }
            skipped.push(index);
        } else {
            to_analyse.push(index);
        }
    }

    Ok((to_analyse, skipped))
}

/// Solves the best two alternatives, then folds in the rest one at a time
/// (`pair_comparison.py:135-187`).
///
/// Returns the total sold, the total bought, and one split per entry of `members`.
fn loop_through_pairs<S: Sellable>(
    routes: &mut [S],
    members: &[usize],
    sell_amount: &BigUint,
    gas_prices: &TokenPriceData,
) -> Result<(BigUint, BigUint, Vec<Fraction>), DecompositionError> {
    let mut sender = members[0];
    let mut receiver = members[1];

    // The sender starts holding the whole amount, not just what it could sell on its own.
    // `move_funds` only ever moves funds from sender to receiver, so the pair's total is fixed at
    // whatever the sender starts with: seeding it from `sell_amount()` -- the amount
    // `sort_routes`' probing `decrease_until_sell` happened to settle on -- caps the entire
    // solution at the best-ranked alternative's individual capacity, and the other alternative can
    // never receive more than the sender gives up.
    //
    // defibot seeds `Fraction(sender.sell_amount, sell_amount)` (`pair_comparison.py:145`), a size
    // the sender is known to be able to fill. **That has a cost here.** When neither member can
    // absorb the whole order, the first pass -- which steps by a half -- only tries the whole
    // amount, half of it and nothing; all three fail or sell zero, `best_amount` never leaves zero
    // and CASE #5.2 zeroes both members. The finer passes cannot recover it, because they restart
    // from an optimum that is now zero and `move_funds`' `while split_sender.is_positive()` never
    // runs. Seen on `WETH_to_GNO_500` at three hops, where 27 of 28 branches funnel through one
    // shallow pool and none can fill the order. Fixing that needs the grouping to stop producing
    // 27 such branches, not a different seed -- pairing the best-ranked alternative with the one
    // that routed most was measured and lost that pair outright.
    let mut splits = [Fraction::one(), Fraction::zero()];
    let mut solved = (sender, receiver);
    for step in step_schedule() {
        solved = move_funds(routes, (sender, receiver), sell_amount, gas_prices, &step, &splits)?;
        (sender, receiver) = choose_sender_receiver(routes, solved);
        splits = [
            split_of(routes[sender].sell_amount(), sell_amount),
            split_of(routes[receiver].sell_amount(), sell_amount),
        ];
    }

    let mut next_sell_amount = sell_amount.clone();
    for &next_route in &members[2..] {
        let (incumbent, amount) = next_route_to_solve(routes, solved, &next_sell_amount);
        next_sell_amount = amount;

        // The incumbent sends only if it actually bought something; otherwise the newcomer starts
        // holding the whole amount (`pair_comparison.py:162-166`).
        let (mut sender, mut receiver) = if routes[incumbent].buy_amount().is_zero() {
            (next_route, incumbent)
        } else {
            (incumbent, next_route)
        };

        let mut splits = [Fraction::one(), Fraction::zero()];
        for step in step_schedule() {
            solved = move_funds(
                routes,
                (sender, receiver),
                &next_sell_amount,
                gas_prices,
                &step,
                &splits,
            )?;
            (sender, receiver) = choose_sender_receiver(routes, solved);
            splits = [
                split_of(routes[sender].sell_amount(), &next_sell_amount),
                split_of(routes[receiver].sell_amount(), &next_sell_amount),
            ];
        }
    }

    let mut sold = BigUint::zero();
    let mut bought = BigUint::zero();
    let mut final_splits = Vec::with_capacity(members.len());
    for &index in members {
        sold += routes[index].sell_amount();
        bought += routes[index].buy_amount();
        // `split_of` limits the denominator, which is defibot's
        // `limit_denominator(SPLIT_PRECISION)` at `pair_comparison.py:181-183`.
        final_splits.push(split_of(routes[index].sell_amount(), sell_amount));
    }

    Ok((sold, bought, final_splits))
}

/// The step schedule as exact rationals.
fn step_schedule() -> impl Iterator<Item = BigRational> {
    STEPS
        .into_iter()
        .map(|(numerator, denominator)| {
            BigRational::new(BigInt::from(numerator), BigInt::from(denominator))
        })
}

/// Picks the alternative to compare against the next newcomer (`pair_comparison.py:190-218`).
///
/// When both members of the pair traded, the smaller one is carried forward with only the amount it
/// holds — the larger one has already been priced against it. When at most one traded, the pair as
/// a whole is under-filled, so the first one is carried forward with the full amount to see whether
/// the newcomer can take what neither could.
fn next_route_to_solve<S: Sellable>(
    routes: &[S],
    pair: (usize, usize),
    sell_amount: &BigUint,
) -> (usize, BigUint) {
    let (first, second) = pair;
    let traded = usize::from(!routes[first].buy_amount().is_zero()) +
        usize::from(!routes[second].buy_amount().is_zero());

    if traded == 2 {
        (second, routes[second].sell_amount().clone())
    } else {
        (first, sell_amount.clone())
    }
}

/// One-dimensional line search moving funds from `sender` to `receiver`
/// (`pair_comparison.py:221-380`).
///
/// Walks the split grid `split_step` at a time from the initial splits and keeps the best total
/// bought net of gas. It **stops at the first decrease**, which assumes the total is unimodal in
/// the split. Gas costs (a route only becomes worth activating past some size) and tick boundaries
/// (a concentrated-liquidity pool's price is piecewise) both break that assumption, so what comes
/// back is a local optimum, not a global one. Behaviour is kept as defibot has it: an exhaustive
/// walk costs a simulation per grid point and the schedule has five passes.
///
/// Returns the pair with the larger seller first.
#[allow(clippy::too_many_arguments, reason = "one-to-one with the ported signature")]
fn move_funds<S: Sellable>(
    routes: &mut [S],
    pair: (usize, usize),
    sell_amount: &BigUint,
    gas_prices: &TokenPriceData,
    split_step: &BigRational,
    initial_splits: &[Fraction; 2],
) -> Result<(usize, usize), DecompositionError> {
    let (sender, receiver) = pair;
    let start_sender = initial_splits[0].as_ratio().clone();
    let start_receiver = initial_splits[1].as_ratio().clone();

    let mut split_sender = start_sender.clone();
    let mut step_count = BigRational::zero();
    let one = BigRational::one();

    let mut best_amount = BigInt::zero();
    let mut best_split = (start_sender.clone(), start_receiver.clone());
    // Deliberately not reset per iteration: defibot lets the previous iteration's value stand when
    // the receiver fails to sell, and CASE #6 below reads it.
    let mut bought_receiver = BigInt::zero();

    while split_sender.is_positive() {
        let offset = split_step * &step_count;
        let split_receiver = &start_receiver + &offset;
        split_sender = &start_sender - &offset;
        step_count += &one;

        // The loop condition tests the *previous* iteration's split, so the last pass can compute a
        // negative one. defibot hands that negative amount to the pool, which raises and is caught
        // as CASE #3; skipping it directly is the same control flow without the wasted call.
        if split_sender.is_negative() {
            continue;
        }

        let sender_amount = scale(sell_amount, &split_sender);
        let bought_sender = match sell_with_gas(&mut routes[sender], &sender_amount, gas_prices) {
            Ok(amount) => amount,
            // CASE #3: the sender cannot fill this size. The next pass asks it for less.
            Err(error) if error.is_recoverable() => continue,
            Err(error) => return Err(error),
        };

        let receiver_amount = scale(sell_amount, &split_receiver);
        match sell_with_gas(&mut routes[receiver], &receiver_amount, gas_prices) {
            Ok(amount) => bought_receiver = amount,
            // CASE #4: the receiver cannot fill this size, and every later pass only asks it for
            // more. The stale `bought_receiver` is what defibot compares against below.
            Err(error) if error.is_recoverable() => {}
            Err(error) => return Err(error),
        }

        let total_bought = &bought_sender + &bought_receiver;

        if best_amount.is_zero() || total_bought > best_amount {
            best_amount = total_bought;
            best_split = (split_sender.clone(), split_receiver.clone());
            continue;
        }
        if total_bought >= best_amount {
            continue;
        }

        // CASE #1: past the optimum. Settle on the best split seen.
        if best_split.1.is_zero() {
            // CASE #2: the sender keeps everything, so the receiver is out of the solution.
            return settle_on_sender(routes, pair, sell_amount, &best_split.0);
        }
        if bought_receiver.is_zero() {
            // CASE #6: the receiver never traded, so the pair cannot absorb the whole amount and
            // the receiver is out of the solution.
            return settle_on_sender(routes, pair, sell_amount, &best_split.0);
        }

        decrease_until_sell(&scale(sell_amount, &best_split.0), |amount| {
            routes[sender].sell(amount)
        })?;
        decrease_until_sell(&scale(sell_amount, &best_split.1), |amount| {
            routes[receiver].sell(amount)
        })?;
        return Ok(order_by_sell_amount(routes, pair));
    }

    // CASE #5: the grid ran out without the total ever decreasing.
    if best_amount.is_zero() {
        // CASE #5.2: neither alternative has liquidity we can use.
        routes[sender].sell(&BigUint::zero())?;
        routes[receiver].sell(&BigUint::zero())?;
        return Ok((sender, receiver));
    }
    // CASE #5.1: the receiver ended up with everything, so it becomes the sender of the next pass.
    decrease_until_sell(&scale(sell_amount, &best_split.0), |amount| routes[sender].sell(amount))?;
    decrease_until_sell(&scale(sell_amount, &best_split.1), |amount| {
        routes[receiver].sell(amount)
    })?;
    Ok((receiver, sender))
}

/// Gives the sender `split` of the amount and zeroes the receiver (`pair_comparison.py:348-351`).
fn settle_on_sender<S: Sellable>(
    routes: &mut [S],
    pair: (usize, usize),
    sell_amount: &BigUint,
    split: &BigRational,
) -> Result<(usize, usize), DecompositionError> {
    let (sender, receiver) = pair;
    routes[sender].sell(&scale(sell_amount, split))?;
    routes[receiver].sell(&BigUint::zero())?;
    Ok((sender, receiver))
}

/// Orders a pair by descending sell amount, keeping the given order on a tie
/// (`pair_comparison.py:362-366`).
fn order_by_sell_amount<S: Sellable>(routes: &[S], pair: (usize, usize)) -> (usize, usize) {
    let (first, second) = pair;
    if routes[first].sell_amount() >= routes[second].sell_amount() {
        (first, second)
    } else {
        (second, first)
    }
}

/// Sells and reports the buy amount net of gas (`routes/interface.py:82-85`).
///
/// Signed, because a route can cost more gas than it buys.
fn sell_with_gas<S: Sellable>(
    route: &mut S,
    amount: &BigUint,
    gas_prices: &TokenPriceData,
) -> Result<BigInt, DecompositionError> {
    let buy_token = route.buy_token().clone();
    let (bought, gas) = route.sell(amount)?;
    let cost = gas_prices.cost_in_token(&gas, &buy_token.address);
    Ok(BigInt::from(bought) - BigInt::from(cost))
}

/// Decides which way funds should move next (`pair_comparison.py:383-405`).
///
/// Funds always flow from the worse executed price to the better one. A zero executed price means
/// the alternative has not been tried at this size, which makes it the receiver — the search wants
/// to find out what it would do with some volume.
fn choose_sender_receiver<S: Sellable>(routes: &[S], pair: (usize, usize)) -> (usize, usize) {
    let (first, second) = pair;
    let (first_price, second_price) =
        (routes[first].executed_price(), routes[second].executed_price());

    // A zero first price and a worse first price both make the first the receiver; defibot spells
    // them as separate branches (`pair_comparison.py:396-402`).
    if second_price == 0.0 {
        (first, second)
    } else if first_price == 0.0 || first_price > second_price {
        (second, first)
    } else {
        (first, second)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use num_traits::One;
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

    use super::*;
    use crate::{
        algorithm::{
            decomposition::components::{ParallelRoute, Pool, Route, SellLimitKind, SequenceRoute},
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

    fn pool(id: &str, reserve_a: u64, reserve_b: u64) -> Pool {
        Pool::new(
            id.to_string(),
            Arc::new(token(0x0A, "A")),
            Arc::new(token(0x0B, "B")),
            SellLimitKind::Enforced,
            Box::new(ConstantProductSim {
                reserve_0: whole(reserve_a),
                reserve_1: whole(reserve_b),
                gas: 100_000,
            }),
            None,
        )
    }

    /// A hop over `pools`, unsolved.
    fn hop(pools: Vec<Pool>) -> ParallelRoute {
        ParallelRoute::new(
            pools
                .into_iter()
                .map(Route::pool)
                .collect(),
        )
        .expect("hop has pools")
    }

    /// A one-hop A -> B chain over a single pool, with the hop's split already set.
    fn route(id: &str, reserve_a: u64, reserve_b: u64) -> SequenceRoute {
        let mut hop = hop(vec![pool(id, reserve_a, reserve_b)]);
        hop.set_splits(vec![Fraction::one()])
            .expect("one split for one pool");
        SequenceRoute::new(vec![hop]).expect("one hop is a chain")
    }

    /// A one-hop route over `pool_count` identical pools where every pool has been sold on but only
    /// the first carries a split.
    ///
    /// This is what a hop-level split search leaves behind: the losing pools keep the gas of their
    /// trial sells and end on a zero split. It is the state that separates `gas` from
    /// `minimum_gas`.
    fn route_with_inactive_pools(
        reserve_a: u64,
        reserve_b: u64,
        pool_count: usize,
        trial: &BigUint,
    ) -> SequenceRoute {
        let pools = (0..pool_count)
            .map(|index| pool(&format!("p{index}"), reserve_a, reserve_b))
            .collect();
        let mut hop = hop(pools);
        for leg in hop.children_mut() {
            leg.sell(trial)
                .expect("every pool absorbs the trial amount");
        }

        let mut splits = vec![Fraction::zero(); pool_count];
        splits[0] = Fraction::one();
        hop.set_splits(splits)
            .expect("one split per pool");
        SequenceRoute::new(vec![hop]).expect("one hop is a chain")
    }

    fn free_gas(gas_price_wei: &BigUint) -> TokenPriceData {
        TokenPriceData::new(gas_price_wei.clone(), None)
    }

    fn optimize<S: Sellable>(
        routes: &mut [S],
        sell_amount: &BigUint,
        gas_prices: &TokenPriceData,
    ) -> SplitSolution {
        split_by_pair_comparison(routes, sell_amount, gas_prices)
            .expect("pair comparison succeeds on well-formed pools")
    }

    fn splits_sum(solution: &SplitSolution) -> BigRational {
        solution
            .splits
            .iter()
            .fold(BigRational::zero(), |total, split| total + split.as_ratio())
    }

    #[test]
    fn test_two_equal_pools_split_evenly() {
        // Two identical constant-product pools are symmetric, so the optimum is exactly half each.
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        let half = Fraction::from_ratio(1, 2).expect("non-zero denominator");
        assert_eq!(solution.splits, vec![half.clone(), half]);
        assert_eq!(solution.sold, sell_amount);
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
    fn test_two_unequal_pools_favour_the_deeper_one() {
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
    fn test_pool_that_cannot_absorb_its_allocation_is_backed_off() {
        // ConstantProductSim caps a sell at half the input reserve, so the small pool can take at
        // most 5 whole tokens however the search allocates.
        let mut routes = vec![route("big", 1_000_000, 1_000_000), route("small", 10, 10)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert!(routes[1].sell_amount() <= &whole(5));
        assert!(!solution.bought.is_zero());
    }

    /// Runs the ranking and pruning steps the way `split_by_pair_comparison` does, and reports the
    /// partition. The end-to-end split is a poor probe for pruning — the line search often lands on
    /// the same answer by a different route — so the partition is asserted directly.
    fn partition<S: Sellable>(
        routes: &mut [S],
        sell_amount: &BigUint,
        gas_prices: &TokenPriceData,
    ) -> (Vec<usize>, Vec<usize>) {
        let sell_token = routes[0].sell_token().clone();
        let ranked =
            sort_routes(routes, sell_amount, gas_prices, &token(0x0B, "B")).expect("ranks");
        prune(routes, &ranked, sell_amount, gas_prices, &sell_token, &token(0x0B, "B"))
            .expect("prunes")
    }

    #[test]
    fn test_pruning_drops_a_route_that_cannot_beat_the_incumbent() {
        // The second pool's spot price is 0.5, below what the first realises even with impact, so
        // the optimistic bound rules it out and the pair search never sees it.
        let mut routes =
            vec![route("fair", 1_000_000, 1_000_000), route("bad", 1_000_000, 500_000)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let (to_analyse, skipped) = partition(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert_eq!(to_analyse, vec![0]);
        assert_eq!(skipped, vec![1]);
        assert!(routes[1].sell_amount().is_zero());
    }

    #[test]
    fn test_pruning_keeps_a_route_that_could_still_help() {
        // Two pools at the same price: the second's zero-impact price beats what the first realises
        // after impact, so it survives.
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let (to_analyse, skipped) = partition(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert_eq!(to_analyse, vec![0, 1]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn test_pruning_charges_only_the_gas_the_splits_activate() {
        // The candidate's hop holds two pools that were both tried, but only one carries a split.
        // Charging it for both would push its optimistic price below the incumbent's realised
        // price and drop a route that can still improve the solution; charging only the activated
        // pool keeps it. Gas is priced so that the second pool's 100_000 units are worth more than
        // the incumbent's price impact, which is what makes the two choices diverge.
        let sell_amount = whole(1);
        let buy_token = token(0x0B, "B");
        let mut routes = vec![
            route("incumbent", 1_000_000, 1_000_000),
            route_with_inactive_pools(1_000_000, 1_000_000, 2, &sell_amount),
        ];
        assert!(
            routes[1].minimum_gas() < routes[1].gas(),
            "setup must leave an inactive pool holding gas"
        );

        let mut token_prices: TokenGasPrices = FxHashMap::default();
        token_prices.insert(
            buy_token.address.clone(),
            Price::new(BigUint::from(10u8).pow(8), BigUint::one()),
        );
        let gas_price_wei = BigUint::one();
        let gas_prices =
            TokenPriceData::new(gas_price_wei.clone(), Some(Arc::new(token_prices.clone())));
        decrease_until_sell(&sell_amount, |amount| routes[0].sell(amount))
            .expect("incumbent sells");

        let ranked = vec![(0, BigInt::one()), (1, BigInt::one())];
        let (to_analyse, skipped) = prune(
            &mut routes,
            &ranked,
            &sell_amount,
            &gas_prices,
            &token(0x0A, "A"),
            &token(0x0B, "B"),
        )
        .expect("prunes");

        assert_eq!(to_analyse, vec![0, 1]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn test_move_funds_tolerates_a_receiver_that_cannot_sell() {
        // The receiver's pool caps at 5 whole tokens, so it fails at every size the search offers
        // it (CASE #4). The search carries on using the sender's numbers alone and, finding the
        // receiver never traded, drops it (CASE #6) instead of propagating the failure.
        let mut routes = vec![route("deep", 1_000_000, 1_000_000), route("tiny", 10, 10)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();
        let half = Fraction::from_ratio(1, 2).expect("non-zero denominator");
        let step = BigRational::new(BigInt::one(), BigInt::from(2u8));

        let pair = move_funds(
            &mut routes,
            (0, 1),
            &sell_amount,
            &free_gas(&gas_price_wei),
            &step,
            &[half.clone(), half],
        )
        .expect("a receiver that cannot sell is not an error");

        assert_eq!(pair, (0, 1));
        assert_eq!(routes[0].sell_amount(), &whole(500));
        assert!(routes[1].sell_amount().is_zero());
    }

    #[test]
    fn test_move_funds_zeroes_a_pair_with_no_usable_liquidity() {
        // Both pools cap at half a whole token, so every grid point the sender is offered fails and
        // the search never records a best (CASE #5.2). Both routes end at zero.
        let mut routes = vec![route("empty_a", 1, 1), route("empty_b", 1, 1)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();
        let step = BigRational::new(BigInt::one(), BigInt::from(2u8));

        let pair = move_funds(
            &mut routes,
            (0, 1),
            &sell_amount,
            &free_gas(&gas_price_wei),
            &step,
            &[Fraction::one(), Fraction::zero()],
        )
        .expect("a pair with no liquidity is not an error");

        assert_eq!(pair, (0, 1));
        assert!(routes[0].sell_amount().is_zero());
        assert!(routes[1].sell_amount().is_zero());
        assert!(routes[0].buy_amount().is_zero());
        assert!(routes[1].buy_amount().is_zero());
    }

    #[test]
    fn test_gas_cost_kills_a_marginal_route() {
        // Two pools that split evenly when gas is free. Pricing gas at one whole buy token per gas
        // unit makes a 100_000-gas swap cost more than either route buys, so the second route's
        // score floors at zero and it is dropped before the search.
        let buy_token = token(0x0B, "B");
        let mut token_prices: TokenGasPrices = FxHashMap::default();
        token_prices.insert(buy_token.address.clone(), Price::new(unit(), BigUint::one()));
        let gas_price_wei = BigUint::one();
        let sell_amount = whole(1);
        let pools = || vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];

        let free = optimize(&mut pools(), &sell_amount, &free_gas(&BigUint::zero()));
        let mut charged_routes = pools();
        let charged = optimize(
            &mut charged_routes,
            &sell_amount,
            &TokenPriceData::new(gas_price_wei.clone(), Some(Arc::new(token_prices.clone()))),
        );

        assert!(!free.splits[1].is_zero(), "gas-free control should use both routes");
        assert_eq!(charged.splits[1], Fraction::zero());
        assert!(charged_routes[1]
            .sell_amount()
            .is_zero());
    }

    #[test]
    fn test_splits_sum_below_one_when_liquidity_is_short() {
        // Both pools cap at 5 whole tokens, so a 1_000-token order cannot be filled and the
        // shortfall has to stay visible in the splits.
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
    }

    #[test]
    fn test_single_route_sells_everything() {
        let mut routes = vec![route("a", 1_000_000, 1_000_000)];
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert_eq!(solution.splits, vec![Fraction::one()]);
        assert_eq!(solution.sold, sell_amount);
    }

    #[test]
    fn test_no_routes() {
        let mut routes: Vec<Route> = Vec::new();
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &whole(1_000), &free_gas(&gas_price_wei));

        assert!(solution.splits.is_empty());
        assert!(solution.sold.is_zero());
    }

    #[test]
    fn test_zero_sell_amount_is_rejected() {
        let mut routes = vec![route("a", 1_000_000, 1_000_000), route("b", 1_000_000, 1_000_000)];
        let gas_price_wei = BigUint::zero();

        let error =
            split_by_pair_comparison(&mut routes, &BigUint::zero(), &free_gas(&gas_price_wei))
                .expect_err("a zero sell amount has no splits");

        assert!(matches!(error, DecompositionError::InvalidStructure { .. }));
    }

    #[test]
    fn test_three_pools_absorb_one_at_a_time() {
        let mut routes = vec![
            route("a", 1_000_000, 1_000_000),
            route("b", 1_000_000, 1_000_000),
            route("c", 1_000_000, 1_000_000),
        ];
        let sell_amount = whole(300_000);
        let gas_price_wei = BigUint::zero();

        let solution = optimize(&mut routes, &sell_amount, &free_gas(&gas_price_wei));

        assert!(solution
            .splits
            .iter()
            .all(|split| !split.is_zero()));
        assert!(splits_sum(&solution) <= BigRational::one());
    }

    #[test]
    fn test_optimizer_splits_a_hop_over_its_pools() {
        // The same optimizer, one level down: the alternatives are a hop's pools, not branches.
        let mut hop = hop(vec![pool("a", 1_000_000, 1_000_000), pool("b", 1_000_000, 1_000_000)]);
        let sell_amount = whole(1_000);
        let gas_price_wei = BigUint::zero();

        let solution = {
            let legs = hop.children_mut();
            optimize(legs, &sell_amount, &free_gas(&gas_price_wei))
        };

        let half = Fraction::from_ratio(1, 2).expect("non-zero denominator");
        assert_eq!(solution.splits, vec![half.clone(), half]);
        hop.set_splits(solution.splits)
            .expect("one split per pool");
        assert!(hop.solved());
    }
}
