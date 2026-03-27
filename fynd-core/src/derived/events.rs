//! Events broadcast by ComputationManager when derived data is updated.
//!
//! Workers subscribe to these events to track readiness of derived data
//! computations before solving.

use super::computation::{ComputationId, FailedItem};

/// Events broadcast when derived data computations complete.
///
/// Workers use these events to update their `ReadinessTracker` and determine
/// when required computations are ready.
#[derive(Debug, Clone)]
pub enum DerivedDataEvent {
    /// A new block has started, clearing previous readiness state.
    ///
    /// Workers should clear their ready set when receiving this event.
    NewBlock {
        /// The new block number.
        block: u64,
    },

    /// A computation completed successfully for a block.
    ///
    /// Workers should mark this computation as ready in their tracker.
    ComputationComplete {
        /// Which computation completed.
        computation_id: ComputationId,
        /// Block number this computation was performed for.
        block: u64,
        /// Items that failed during this computation (empty = full success).
        failed_items: Vec<FailedItem>,
    },

    /// A computation failed for a block and will not produce a result.
    ///
    /// Workers with `require_fresh` requirements use this to exit immediately
    /// rather than waiting until timeout.
    ComputationFailed {
        /// Which computation failed.
        computation_id: ComputationId,
        /// Block number this computation was attempted for.
        block: u64,
    },
}
