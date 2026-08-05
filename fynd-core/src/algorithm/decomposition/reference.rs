//! The reference route: a deliberately small, safe solution.
//!
//! Port of `_build_reference_solution` (`order_solver.py:326-385`) and the part of `_solve` that
//! consumes it (`:226-248`, `:300-310`).
//!
//! The reference is one direct pool set plus one two-hop path through a well-connected intermediate
//! token, solved the ordinary way. It does three jobs:
//!
//! 1. Its post-trade marginal price is the floor candidate paths must clear (`:240-242`).
//! 2. It is the answer if the full candidate subgraph cannot be built at all (`:244-248`).
//! 3. It is the baseline the candidate must beat on output net of gas (`:304-308`), which
//!    [`choose_solution`](super::solve::choose_solution) applies.
//!
//! # Deviations from defibot
//!
//! * defibot hard-codes WETH as the intermediate token (`constants.WRAPPED_TOKEN`,
//!   `order_solver.py:344`). Fynd runs on chains whose wrapped native token differs and whose
//!   deepest connector is not always the wrapped native token at all, so the token is a parameter.
//!   Pass the chain's most liquid connector; the reference is only as safe as that choice.
//! * defibot prints the route scheme to stdout from inside the warning path (`:234`). A solver
//!   worker has no stdout worth writing to; the warning carries the same information through
//!   `tracing`.

use std::time::Instant;

use num_bigint::BigUint;
use tracing::{debug, warn};
use tycho_simulation::tycho_core::models::token::Token;

use crate::{
    algorithm::decomposition::{
        components::{Branch, DecompositionError, SequentialRoute, SolutionGraph},
        graph_build::{build_routes_subgraph, SubgraphParams},
        optimizers::{GasPrices, SplitOptimizerT},
        solve::solve_graph,
        token_graph::{SearchBounds, TokenGraph},
    },
    derived::types::ComponentDepths,
    feed::market_data::MarketState,
    AlgorithmError,
};

/// Inputs to [`build_reference_solution`].
pub(crate) struct ReferenceParams<'a> {
    /// Token the order sells.
    pub(crate) sell_token: &'a Token,
    /// Token the order buys.
    pub(crate) buy_token: &'a Token,
    /// Token the two-hop leg of the reference goes through.
    ///
    /// defibot uses the chain's wrapped native token. What actually matters is that the token is
    /// deeply connected to everything else on the chain, because the whole point of the reference
    /// is that it almost always exists. When it is one of the order's own endpoints the two-hop
    /// leg is skipped and the reference is direct pools only (`order_solver.py:377-380`).
    pub(crate) intermediate_token: &'a Token,
    /// Cap on parallel pools kept per hop.
    pub(crate) max_routes: usize,
    /// Instant the reference legs' path enumeration stops at, or `None` to run it out.
    ///
    /// Each leg is a one-hop search between a fixed token pair, so the bound is a backstop against
    /// a pathological graph rather than something the reference is expected to hit.
    pub(crate) deadline: Option<Instant>,
}

/// Cap on paths enumerated per reference leg.
///
/// A leg is a single hop between two fixed tokens, so every path it can find is one edge and the
/// count is the number of pools on that pair. The bound only exists so the reference cannot be the
/// thing that runs the solve clock out.
const REFERENCE_MAX_PATHS: usize = 1024;

/// Builds and solves the reference route, or `None` when no safe route exists.
///
/// A `None` return is not an error: an order between two thinly connected tokens legitimately has
/// neither a direct pool nor a path through the intermediate token, and the caller then falls back
/// to the candidate subgraph alone.
///
/// The solved graph's [`SolutionGraph::new_marginal_price`] is the price floor for candidate
/// filtering. defibot drops the reference entirely when that price is unavailable (`:228-235`),
/// because a reference that cannot state a price can neither filter nor be compared against; the
/// same check is applied here before returning.
///
/// # Errors
///
/// [`AlgorithmError`] for a structural failure while assembling or solving. Missing paths are
/// reported as `Ok(None)`.
pub(crate) fn build_reference_solution<O: SplitOptimizerT>(
    graph: &TokenGraph<'_>,
    market: &MarketState,
    depths: Option<&ComponentDepths>,
    params: &ReferenceParams<'_>,
    sell_amount: &BigUint,
    optimizer: &O,
    gas_prices: &GasPrices,
) -> Result<Option<SolutionGraph>, AlgorithmError> {
    let Some(mut reference) = build_reference_graph(graph, market, depths, params)? else {
        return Ok(None);
    };

    // The reference is one or two branches, so the level split is the same optimizer as below it.
    solve_graph(&mut reference, sell_amount, optimizer, optimizer, gas_prices)
        .map_err(cast_error)?;

    if reference.new_marginal_price().is_none() {
        warn!(
            sell_token = %params.sell_token.address,
            buy_token = %params.buy_token.address,
            branches = reference.branches().len(),
            "reference route has no post-trade marginal price; solving without a reference"
        );
        return Ok(None);
    }

    Ok(Some(reference))
}

/// Builds the unsolved reference graph (`order_solver.py:344-380`).
fn build_reference_graph(
    graph: &TokenGraph<'_>,
    market: &MarketState,
    depths: Option<&ComponentDepths>,
    params: &ReferenceParams<'_>,
) -> Result<Option<SolutionGraph>, AlgorithmError> {
    let intermediate = &params.intermediate_token.address;
    if &params.sell_token.address == intermediate || &params.buy_token.address == intermediate {
        // The two-hop leg would revisit an endpoint. defibot still asks for depth 2 here
        // (`:378-380`), which admits a path through some other token — the reference stays a
        // small subgraph because it is built without a minimum price and capped like any other.
        return subgraph(graph, market, depths, params, params.sell_token, params.buy_token, 2);
    }

    let direct = subgraph(graph, market, depths, params, params.sell_token, params.buy_token, 1)?;
    let through = through_intermediate(graph, market, depths, params)?;

    let mut branches: Vec<Branch> = Vec::new();
    if let Some(direct) = direct {
        branches.extend(direct.into_branches());
    }
    if let Some(through) = through {
        branches.push(Branch::from_route(through).map_err(cast_error)?);
    }
    if branches.is_empty() {
        return Ok(None);
    }

    SolutionGraph::new(branches, Vec::new())
        .map(Some)
        .map_err(cast_error)
}

/// The `sell -> intermediate -> buy` branch, or `None` when either leg is missing
/// (`order_solver.py:353-370`).
fn through_intermediate(
    graph: &TokenGraph<'_>,
    market: &MarketState,
    depths: Option<&ComponentDepths>,
    params: &ReferenceParams<'_>,
) -> Result<Option<SequentialRoute>, AlgorithmError> {
    let first =
        subgraph(graph, market, depths, params, params.sell_token, params.intermediate_token, 1)?;
    let second =
        subgraph(graph, market, depths, params, params.intermediate_token, params.buy_token, 1)?;
    let (Some(first), Some(second)) = (first, second) else {
        return Ok(None);
    };

    let (Some(first), Some(second)) = (single_hop(first), single_hop(second)) else {
        // A one-hop subgraph between a fixed token pair has exactly one token sequence and so
        // exactly one branch of one leg. Anything else means the builder changed shape; the
        // reference is a best-effort fallback, so it is dropped rather than failing the order.
        warn!("reference leg was not a single hop; skipping the route through the intermediate");
        return Ok(None);
    };

    let tokens = vec![
        params.sell_token.clone(),
        params.intermediate_token.clone(),
        params.buy_token.clone(),
    ];
    SequentialRoute::new(tokens, vec![first, second])
        .map(Some)
        .map_err(cast_error)
}

/// The single leg of a one-hop subgraph, or `None` when it has any other shape.
///
/// A one-hop subgraph's single branch has no tails, so its head *is* the leg.
fn single_hop(graph: SolutionGraph) -> Option<super::components::Hop> {
    let mut branches = graph.into_branches();
    if branches.len() != 1 {
        return None;
    }
    let branch = branches.remove(0);
    if !branch.sequences().is_empty() {
        return None;
    }
    Some(branch.into_hop())
}

/// One leg's subgraph, or `None` when it has no route.
///
/// defibot catches `NoPathsFound` and `UnavailableRouteError` around each leg
/// (`order_solver.py:350-351`, `:369-370`).
fn subgraph(
    graph: &TokenGraph<'_>,
    market: &MarketState,
    depths: Option<&ComponentDepths>,
    params: &ReferenceParams<'_>,
    sell_token: &Token,
    buy_token: &Token,
    max_hops: usize,
) -> Result<Option<SolutionGraph>, AlgorithmError> {
    let bounds =
        SearchBounds { max_hops, max_paths: REFERENCE_MAX_PATHS, deadline: params.deadline };
    let paths = graph.paths_between(&sell_token.address, &buy_token.address, &bounds);
    // The reference is the floor other candidates must clear, so it has none of its own.
    let subgraph_params = SubgraphParams { max_routes: params.max_routes, minimum_price: 0.0 };
    let subgraph = build_routes_subgraph(market, depths, &subgraph_params, &paths)?;
    if subgraph.is_none() {
        debug!(from = %sell_token.address, to = %buy_token.address, "reference leg unavailable");
    }
    Ok(subgraph)
}

/// Wraps a structural failure from the solution types as an algorithm error.
fn cast_error(error: DecompositionError) -> AlgorithmError {
    AlgorithmError::Other(format!("decomposition reference route failed: {error}"))
}

#[cfg(test)]
#[path = "tests/reference_tests.rs"]
mod tests;
