//! The reference route: a deliberately small, safe solution.
//!
//! Port of `_build_reference_solution` (`order_solver.py:326-385`) and the part of `_solve` that
//! consumes it (`:226-248`, `:300-310`).
//!
//! The reference is the direct pools plus the paths through a well-connected intermediate token,
//! solved the ordinary way. It does three jobs:
//!
//! 1. Its post-trade marginal price is the floor candidate paths must clear (`:240-242`).
//! 2. It is the answer if the full candidate subgraph cannot be built at all (`:244-248`).
//! 3. It is the baseline the candidate must beat on output net of gas (`:304-308`), which
//!    [`choose_solution`](super::solve::choose_solution) applies.
//!
//! The paths themselves are not searched here. `DecompositionAlgorithm::search_graph` enumerates
//! them alongside the candidate paths, under one pass over the graph, and hands them over.
//!
//! # Deviations from defibot
//!
//! * defibot hard-codes WETH as the intermediate token (`constants.WRAPPED_TOKEN`,
//!   `order_solver.py:344`). Fynd runs on chains whose wrapped native token differs and whose
//!   deepest connector is not always the wrapped native token at all, so the intermediates are the
//!   graph's own highest-degree tokens.
//! * defibot prints the route scheme to stdout from inside the warning path (`:234`). A solver
//!   worker has no stdout worth writing to; the warning carries the same information through
//!   `tracing`.

use tracing::{debug, warn};

use crate::{
    algorithm::decomposition::{
        components::{DecompositionError, DecompositionGraph},
        graph_build::{build_decomposition_graph, SubgraphParams},
        models::{DirectPath, TokenPriceData},
        solve::solve_graph,
        SolveRequest,
    },
    feed::market_data::MarketState,
};

/// Builds and solves the reference route, or `None` when no safe route exists.
///
/// A `None` return is not an error: an order between two thinly connected tokens legitimately has
/// neither a direct pool nor a path through an intermediate token, and the caller then falls back
/// to the candidate subgraph alone.
///
/// `paths` is the reference path set — the direct pools and the routes through the connector
/// tokens. `max_routes` caps the parallel alternatives kept, as it does for the candidate graph.
///
/// The solved graph's [`DecompositionGraph::new_marginal_price`] is the price floor for candidate
/// filtering. defibot drops the reference entirely when that price is unavailable (`:228-235`),
/// because a reference that cannot state a price can neither filter nor be compared against; the
/// same check is applied here before returning.
///
/// # Errors
///
/// [`DecompositionError`] for a structural failure while building or solving. An absent reference
/// is reported as `Ok(None)`.
/// CZ: use most liquid to find single best path at 2 hops and maybe also at 3 hops
pub(crate) fn solve_reference_solution(
    solve_input: &SolveRequest,
    paths: Vec<DirectPath>,
    max_routes: usize,
    market_state: &MarketState,
    gas_prices: &TokenPriceData,
) -> Result<Option<DecompositionGraph>, DecompositionError> {
    // The reference is the floor other candidates must clear, so it has none of its own.
    let params = SubgraphParams { max_routes, minimum_price: 0.0 };
    let mut reference = match build_decomposition_graph(
        market_state,
        solve_input.depths.as_ref(),
        &params,
        paths,
    ) {
        Ok(reference) => reference,
        Err(DecompositionError::GraphBuildFailure) => {
            debug!(
                sell_token = %solve_input.order.token_in(),
                buy_token = %solve_input.order.token_out(),
                "no reference route between these tokens"
            );
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    solve_graph(
        &mut reference,
        solve_input.order.amount(),
        solve_input.split_optimizers,
        gas_prices,
    )?;

    if reference.new_marginal_price().is_none() {
        warn!(
            sell_token = %solve_input.order.token_in(),
            buy_token = %solve_input.order.token_out(),
            branches = reference.sequences.len(),
            "reference route has no post-trade marginal price; solving without a reference"
        );
        return Ok(None);
    }

    Ok(Some(reference))
}

#[cfg(test)]
#[path = "tests/reference_tests.rs"]
mod tests;
