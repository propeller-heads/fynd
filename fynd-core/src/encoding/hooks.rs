//! Embedder-supplied steps that run inside encoding.
//!
//! An embedder attaches them per request with [`EncodingOptions::with_solution_hook`] and
//! [`EncodingOptions::with_calldata_hook`]. Each hook runs on each quote that reaches its step, a
//! retried candidate included.
//!
//! A hook error fails only that quote, with [`QuoteStatus::EncodingFailed`], like any encoding
//! error: it counts in the encoding failure metrics, and the retry step then encodes the order's
//! next non-RFQ candidate.
//!
//! A hook runs with the tycho encode on the quote's blocking thread, inside the encode deadline
//! when one is set. Only retried candidates have one: a best candidate's slow hook delays the
//! response. A hook past the deadline fails its quote but keeps running; only its result is
//! dropped. A hook may run for a quote that is later discarded (a failed encode, a retried
//! candidate), so it must have no side effects.
//!
//! [`EncodingOptions::with_solution_hook`]: crate::EncodingOptions::with_solution_hook
//! [`EncodingOptions::with_calldata_hook`]: crate::EncodingOptions::with_calldata_hook
//! [`QuoteStatus::EncodingFailed`]: crate::QuoteStatus::EncodingFailed

use std::{fmt::Debug, panic::RefUnwindSafe};

use tycho_execution::encoding::models::{EncodedSolution, Solution};

use crate::{OrderQuote, SolveError};

/// Adjusts the tycho `Solution` built for a quote, just before it is encoded.
///
/// It receives the `Solution` with the fee-adjusted `min_amount_out` already set. The router call
/// reads `expected_amount_out` from the returned `Solution`.
///
/// The hook may raise `expected_amount_out`, and may set the subsidy terms
/// (`Solution::with_subsidy`). Lowering `expected_amount_out`, or any other change, fails the
/// quote.
///
/// The quote reports the hooked `expected_amount_out` as `amount_out` and moves
/// `amount_out_net_gas` by the same difference. The fee breakdown's `router_fee` and `client_fee`
/// are computed on the hooked amount, because the router takes both fees on the full output.
/// `min_amount_received`, `max_slippage` and the per-swap amounts of the route stay as computed
/// before the hook, because the hook's raise is not guaranteed on chain.
pub trait SolutionHook: Debug + Send + Sync + RefUnwindSafe {
    /// Returns the `Solution` to encode for `quote`.
    ///
    /// # Errors
    /// An error fails this quote only.
    fn apply(&self, quote: &OrderQuote, solution: Solution) -> Result<Solution, SolveError>;
}

/// Edits a quote's router calldata after encoding and before simulation.
///
/// `calldata` is the router call, selector included; the `EncodedSolution` byte offsets apply to
/// it. The watermark is appended after the hook, so a hook that decodes or hashes `calldata` gets
/// the same bytes for every client. Simulation runs on the edited calldata.
///
/// The hook may write into the swaps bytes, for example a signature inside one swap. The fee
/// breakdown's swaps hash is computed after the hook, so a client signs its fee over the final
/// swaps. The hook must not change the amounts or the receiver: the client fee signature covers
/// them.
///
/// `calldata` is what the router executes. Its values can differ from `solution`, `encoded` and
/// `quote`, so read them from `calldata`.
pub trait CalldataHook: Debug + Send + Sync + RefUnwindSafe {
    /// Overwrites bytes of `calldata` in place.
    ///
    /// `calldata` is a slice, so its length cannot change: the `EncodedSolution` offsets, the
    /// client fee signature offset and the position of the swaps stay valid.
    ///
    /// # Errors
    /// An error fails this quote only.
    fn apply(
        &self,
        quote: &OrderQuote,
        solution: &Solution,
        encoded: &EncodedSolution,
        calldata: &mut [u8],
    ) -> Result<(), SolveError>;
}

// Losing either auto trait on `EncodingOptions` is a breaking change for embedders.
#[cfg(test)]
const _: fn() = || {
    fn assert_unwind_safe<T: std::panic::UnwindSafe + RefUnwindSafe>() {}
    assert_unwind_safe::<crate::EncodingOptions>();
};
