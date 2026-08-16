//! Failures raised while building or solving a decomposition solution.

use num_bigint::BigUint;
use tycho_simulation::tycho_core::models::Address;

use crate::{algorithm::decomposition::components::*, AlgorithmError};

// ===================== Errors =====================

/// Failures raised while composing or selling on a [`DecompositionGraph`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum DecompositionError {
    /// The requested sell amount exceeds what the route can absorb.
    ///
    /// `limit` is expressed in on-chain units of `token`; callers back off to `limit - 1`. Unlike
    /// defibot (`routes/simple.py:143-147`) the message carries no rendered route scheme —
    /// building one on the hot path is pure cost and defibot has three TODOs asking for its
    /// removal.
    #[error("sell amount exceeds limit {limit} for token {token} (pools: {pools:?})")]
    SellAmountLimit {
        /// Largest amount that can be sold, in on-chain units of `token`.
        limit: BigUint,
        /// Token the limit is denominated in.
        token: Address,
        /// Components responsible for the limit.
        pools: Vec<ComponentId>,
    },

    /// A pool simulation call failed.
    #[error("simulation failed on component {component}: {source}")]
    Simulation {
        /// Component whose simulation failed.
        component: ComponentId,
        /// Underlying simulation failure.
        #[source]
        source: SimulationError,
    },

    /// A hop was asked to sell before its splits were set.
    #[error("hop {token_in} -> {token_out} has no splits; solve it before selling")]
    Unsolved {
        /// Hop input token.
        token_in: Address,
        /// Hop output token.
        token_out: Address,
    },

    /// The structure being built violates one of the fixed-shape invariants.
    #[error("invalid solution structure: {reason}")]
    InvalidStructure {
        /// What was wrong with the input.
        reason: String,
    },
    /// No candidate route survived the build.
    ///
    /// Either the search found no path, or every path it found was filtered out before a branch
    /// could be made from it. The build site logs which.
    #[error("no candidate route could be built")]
    GraphBuildFailure,

    /// A leg of a token sequence ended up with no pool to trade it.
    ///
    /// Only that one sequence is unroutable, so the caller drops it and keeps the rest. Reachable
    /// through `seen_pools`: a pool claimed by an earlier leg cannot serve a later one, and a leg
    /// whose only pool goes that way is left empty.
    #[error("no pool left to trade {token_in} -> {token_out}")]
    EmptyHop {
        /// Leg input token.
        token_in: Address,
        /// Leg output token.
        token_out: Address,
    },

    /// The order cannot be solved as asked.
    #[error("cannot solve this order: {reason}")]
    InvalidInput {
        /// What about the order or the graph makes it unsolvable.
        reason: String,
    },

    /// The market state a solve needs could not be read.
    #[error("could not read market state: {reason}")]
    MarketRead {
        /// Which read failed and why.
        reason: String,
    },

    /// The solve produced no solution to return.
    #[error("no solution to return")]
    SolveError,

    /// A solved graph could not be turned into a `Route`.
    #[error("solution did not assemble into a route: {error}")]
    RouteBuildFailure {
        /// Why the assembly failed.
        #[source]
        error: AlgorithmError,
    },
}

impl DecompositionError {
    /// Whether selling a smaller amount could succeed where this failure occurred.
    ///
    /// Mirrors the exception set `decrease_until_sell` retries on
    /// (`defibot/solver/order_solver/decomposition/utils.py:94-103`): a size limit or a failed pool
    /// simulation may well accept a smaller trade, while a structural problem never will.
    pub(crate) fn is_recoverable(&self) -> bool {
        match self {
            Self::SellAmountLimit { .. } | Self::Simulation { .. } => true,
            Self::Unsolved { .. } |
            Self::InvalidStructure { .. } |
            Self::GraphBuildFailure |
            Self::EmptyHop { .. } |
            Self::InvalidInput { .. } |
            Self::MarketRead { .. } |
            Self::SolveError |
            Self::RouteBuildFailure { .. } => false,
        }
    }
}
