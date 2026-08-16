//! Turning a solved [`DecompositionGraph`] into an encodable Fynd [`Route`].
//!
//! The decomposition's three-level structure and the encoder's flat swap list describe the same
//! trade in different shapes, and they do not agree on amounts by construction. This module owns
//! the conversion and the reconciliation.
//!
//! # The rounding reconciliation
//!
//! Inside the solver every split is applied as `floor(amount * split)`
//! ([`Fraction::apply`](super::components::Fraction::apply)), independently per pool. A hop over
//! `P` pools can therefore route up to `P - 1` on-chain units less than it was handed even when its
//! splits sum to exactly one, and the loss compounds once per level — pinned by
//! `test_hop_sell_loses_up_to_one_unit_per_pool_to_rounding` and
//! `test_graph_sell_loses_rounding_at_every_level` in `route_tests.rs`.
//!
//! The encoder's convention is the opposite: within each set of swaps leaving the same token, all
//! but the last carry an explicit fraction and the last carries `split = 0.0`, meaning *spend the
//! entire remaining balance* (see
//! [`build_split_route`](crate::algorithm::split_primitives::build_split_route)). Nothing is left
//! behind on-chain.
//!
//! Reporting the solver's `buy_amount` next to a route that will spend more input than the solver
//! simulated is exactly the quote-disagrees-with-its-own-transaction failure this reconciliation
//! exists to prevent. So the solver's amounts are **not** carried across this boundary:
//!
//! * The solver's `buy_amount` is used only to *rank* the candidate against the reference
//!   ([`choose_solution`](super::solve::choose_solution)) — a comparison between two numbers
//!   produced the same way, where the shared bias cancels.
//! * The [`RouteResult`] returned to the worker is re-derived from the assembled [`Route`] by
//!   summing the swaps `execute_split_plan` actually simulated. Those swaps are the ones that get
//!   encoded, so the quote and the transaction are the same object by construction, not by
//!   agreement to within a tolerance.
//!
//! The residual difference is therefore between the solver's internal estimate and the reported
//! quote, and it is bounded and signed in a known direction:
//!
//! * **Input.** The solver routes at most `order.amount()` and at least `order.amount() - R`, where
//!   `R` is the sum over emitted swap groups of (group size − 1). The assembled route always routes
//!   exactly `order.amount()`. So the route spends between `0` and `R` units *more* than the solver
//!   simulated. `R` is bounded by the total number of emitted swaps, i.e. under 100 wei on any real
//!   route — sub-dust on every token in existence.
//! * **Output.** The reported output is whatever that extra input actually buys, because it comes
//!   from re-simulating. It is not the solver's number plus a correction.
//!
//! There is a second, larger source of divergence in the same direction, and it is structural
//! rather than arithmetic: the encoder pools every inflow of an intermediate token across branches
//! before re-splitting it, while the solver threads each branch independently. Where two branches
//! share an intermediate token the two models genuinely differ. Re-deriving the quote from the
//! assembled route is what makes that safe as well — the reported number describes the merged
//! execution, which is the one that happens.

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use tracing::debug;

use crate::{
    algorithm::{
        decomposition::{
            components::{Branch, BranchSide, DecompositionGraph, Hop},
            models::TokenPriceData,
        },
        split_primitives::{build_split_route, HopDescriptor, PathAllocation, SimulatedHop},
    },
    feed::market_data::MarketState,
    types::{Order, RouteResult},
    AlgorithmError,
};

/// Upper bound on the linear paths one solution graph may expand into.
///
/// The expansion is a cartesian product over the pools each leg activates, so a three-hop branch
/// whose legs each split four ways is already 64 paths. `build_split_route` merges shared hops, so
/// the expansion cost is paid in assembly rather than in the emitted route — but it is still paid,
/// and an unbounded product would let one pathological solution dominate the solve clock. Paths are
/// kept in descending flow order, so the bound drops the smallest allocations first and
/// `assign_splits_and_amounts` renormalises what is left.
const MAX_ASSEMBLED_PATHS: usize = 256;

/// Share of the order below which a solution is only stretched under protest, with a warning.
///
/// A [`SplitSolution`](super::optimizers::SplitSolution)'s splits deliberately need not sum to one:
/// a shortfall is how the solver says the market could not absorb the whole order
/// (`optimizers/interface.py:26-31`, and every path through
/// [`solve_without_splits`](super::solve::solve_without_splits)). Fynd's [`Route`] has no way to
/// say that — an exact-in route spends `order.amount()` and nothing less, and
/// [`build_split_route`] renormalises the fractions it is given to make that true.
///
/// Stretching is safe for the *quote*: [`build_split_route`] re-simulates every swap at the
/// stretched amount, so the number returned is computed, never extrapolated, and a pool pushed past
/// what it can serve fails the simulation instead of lying. What stretching cannot promise is
/// *quality* — the splits were optimised for a fraction of the order and are unlikely to be optimal
/// for all of it.
///
/// Refusing instead was measurably worse: on the recorded fixture it discarded candidates worth
/// 3–5x the reference and, on two pairs, left no answer at all.
const LOW_ROUTED_FLOW: f64 = 0.999;

/// A path under construction during the cartesian expansion of one branch.
struct PartialPath {
    hops: Vec<HopDescriptor>,
    /// Share of the *order* that reaches this path, not the share of its branch.
    fraction: f64,
}

/// Assembles the solved `graph` into a validated [`Route`] and the [`RouteResult`] describing it.
///
/// The returned result's amounts come from re-simulating the assembled route, never from the
/// solver's own totals — see the module docs.
///
/// # Errors
///
/// [`AlgorithmError::InsufficientLiquidity`] when the solution activates no pool at all, whatever
/// `build_split_route` raises while simulating, and the validation error when the assembled route
/// is not encodable.
pub(crate) fn cast_into_route(
    graph: &DecompositionGraph,
    market: &MarketState,
    order: &Order,
    gas_prices: &TokenPriceData,
) -> Result<RouteResult, AlgorithmError> {
    let allocations = solution_allocations(graph);
    if allocations.is_empty() {
        return Err(AlgorithmError::InsufficientLiquidity);
    }
    let routed_flow: f64 = allocations
        .iter()
        .map(|allocation| allocation.flow_fraction)
        .sum();
    if routed_flow < LOW_ROUTED_FLOW {
        // `build_split_route` renormalises to spend the whole order and re-simulates at the
        // stretched amounts, so the quote stays honest; the split ratios are the part that was
        // optimised for less than the order.
        debug!(
            routed_flow,
            "decomposition solution routes less than the order; stretching it to the full amount"
        );
    }

    let route = build_split_route(&allocations, market, order)?;
    route.validate()?;

    let token_out = order.token_out();
    let mut gross = BigUint::zero();
    let mut gas = BigUint::zero();
    for swap in route.swaps() {
        gas += swap.gas_estimate();
        // Candidate paths are simple and terminate at the buy token, so a swap producing it is
        // always a terminal leg and never an intermediate one.
        if swap.token_out() == token_out {
            gross += swap.amount_out();
        }
    }

    let cost = gas_prices.cost_in_token(&gas, token_out);
    let net = BigInt::from(gross.clone()) - BigInt::from(cost);
    debug!(
        swaps = route.swaps().len(),
        %gross,
        solver_buy_amount = %graph.buy_amount(),
        "decomposition assembled route; quote re-derived from the assembled swaps"
    );

    Ok(RouteResult::new(route, net, gas_prices.gas_price_wei.clone()))
}

/// Expands every branch of the graph into the linear paths the encoder consumes.
///
/// One [`PathAllocation`] per combination of activated pools along a branch, carrying that
/// combination's share of the whole order. Only [`build_split_route`]'s `hops` and `flow_fraction`
/// are read, so the simulation-derived fields are left at zero: `execute_split_plan` recomputes
/// them against the merged, topologically ordered plan.
fn solution_allocations(graph: &DecompositionGraph) -> Vec<PathAllocation> {
    let mut allocations = Vec::new();
    for (branch, split) in graph
        .branches()
        .iter()
        .zip(graph.outer_splits())
    {
        if split.is_zero() {
            continue;
        }
        allocations.extend(branch_allocations(branch, split.to_f64()));
    }

    allocations.sort_by(|left, right| {
        right
            .flow_fraction
            .total_cmp(&left.flow_fraction)
    });
    if allocations.len() > MAX_ASSEMBLED_PATHS {
        debug!(
            paths = allocations.len(),
            kept = MAX_ASSEMBLED_PATHS,
            "decomposition solution expanded past the assembly bound; dropping the smallest flows"
        );
        allocations.truncate(MAX_ASSEMBLED_PATHS);
    }
    allocations
}

/// Expands one branch into linear paths.
///
/// The branch's own hop appears in every path this emits — at the front for
/// [`BranchSide::Head`], at the back for [`BranchSide::Tail`] — and that repetition is exactly the
/// shared prefix or suffix [`build_split_route`] merges back into a single on-chain swap. Emitting
/// it once per sequence here and letting the encoder merge it is what keeps the assembled route's
/// shape identical to the branch's: one swap per pool of that hop, however many sequences feed it.
///
/// Returns nothing when a leg is unsolved or routes nothing — a path with a dead leg carries no
/// flow at all, and emitting its earlier legs would strand tokens at the break.
fn branch_allocations(branch: &Branch, branch_fraction: f64) -> Vec<PathAllocation> {
    match branch.side() {
        BranchSide::Head => hop_first_allocations(branch, branch_fraction),
        BranchSide::Tail => sequences_first_allocations(branch, branch_fraction),
    }
}

/// [`BranchSide::Tail`]: every sequence's activated paths, each continued through the shared hop.
fn sequences_first_allocations(branch: &Branch, branch_fraction: f64) -> Vec<PathAllocation> {
    let tail_pools = activated_pools(branch.hop());
    if tail_pools.is_empty() {
        return Vec::new();
    }

    let mut partials = Vec::new();
    for (sequence, split) in branch
        .sequences()
        .iter()
        .zip(branch.splits())
    {
        if split.is_zero() {
            continue;
        }
        let mut extended =
            vec![PartialPath { hops: Vec::new(), fraction: branch_fraction * split.to_f64() }];

        let mut dead_leg = false;
        for hop in sequence.hops() {
            let activated = activated_pools(hop);
            if activated.is_empty() {
                dead_leg = true;
                break;
            }
            let mut next = Vec::with_capacity(extended.len() * activated.len());
            for partial in &extended {
                for (descriptor, pool_share) in &activated {
                    let mut hops = partial.hops.clone();
                    hops.push(descriptor.clone());
                    next.push(PartialPath { hops, fraction: partial.fraction * pool_share });
                }
            }
            extended = next;
        }
        if dead_leg {
            continue;
        }

        // The shared hop closes every path the sequence produced.
        for partial in &extended {
            for (descriptor, pool_share) in &tail_pools {
                let mut hops = partial.hops.clone();
                hops.push(descriptor.clone());
                partials.push(PartialPath { hops, fraction: partial.fraction * pool_share });
            }
        }
    }
    finish(partials)
}

/// [`BranchSide::Head`]: the hop's activated pools, each continued down every activated path of
/// every sequence carrying flow.
fn hop_first_allocations(branch: &Branch, branch_fraction: f64) -> Vec<PathAllocation> {
    let head = activated_pools(branch.hop());
    if head.is_empty() {
        return Vec::new();
    }
    let heads: Vec<PartialPath> = head
        .into_iter()
        .map(|(descriptor, share)| PartialPath {
            hops: vec![descriptor],
            fraction: branch_fraction * share,
        })
        .collect();

    if branch.sequences().is_empty() {
        return finish(heads);
    }

    let mut partials = Vec::new();
    for (tail, tail_split) in branch
        .sequences()
        .iter()
        .zip(branch.splits())
    {
        if tail_split.is_zero() {
            continue;
        }
        let share = tail_split.to_f64();
        let mut extended: Vec<PartialPath> = heads
            .iter()
            .map(|head| PartialPath { hops: head.hops.clone(), fraction: head.fraction * share })
            .collect();

        let mut dead_leg = false;
        for hop in tail.hops() {
            let activated = activated_pools(hop);
            if activated.is_empty() {
                dead_leg = true;
                break;
            }
            let mut next = Vec::with_capacity(extended.len() * activated.len());
            for partial in &extended {
                for (descriptor, pool_share) in &activated {
                    let mut hops = partial.hops.clone();
                    hops.push(descriptor.clone());
                    next.push(PartialPath { hops, fraction: partial.fraction * pool_share });
                }
            }
            extended = next;
        }
        if !dead_leg {
            partials.extend(extended);
        }
    }

    finish(partials)
}

/// Turns finished partial paths into the allocations [`build_split_route`] consumes.
fn finish(partials: Vec<PartialPath>) -> Vec<PathAllocation> {
    partials
        .into_iter()
        .filter(|partial| partial.fraction > 0.0)
        .map(|partial| PathAllocation {
            hops: partial
                .hops
                .into_iter()
                .map(|descriptor| SimulatedHop {
                    descriptor,
                    amount_out: BigUint::zero(),
                    gas: BigUint::zero(),
                })
                .collect(),
            flow_fraction: partial.fraction,
            amount_in: BigUint::zero(),
            amount_out: BigUint::zero(),
            marginal_price_product: 0.0,
        })
        .collect()
}

/// The pools of `hop` carrying a non-zero split, with that split.
fn activated_pools(hop: &Hop) -> Vec<(HopDescriptor, f64)> {
    hop.pools()
        .iter()
        .zip(hop.splits())
        .filter(|(_, split)| !split.is_zero())
        .map(|(pool, split)| {
            (
                HopDescriptor::new(
                    pool.component_id().clone(),
                    hop.token_in().clone(),
                    hop.token_out().clone(),
                ),
                split.to_f64(),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "tests/assemble_tests.rs"]
mod tests;
