//! Failures raised while building or solving a decomposition solution.

use num_bigint::BigUint;
use tycho_simulation::tycho_core::models::Address;

use crate::algorithm::decomposition::components::*;

// ===================== Errors =====================

/// Failures raised while composing or selling on a [`SolutionGraph`].
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
            Self::Unsolved { .. } | Self::InvalidStructure { .. } => false,
        }
    }
}
