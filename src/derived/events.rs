//! Events broadcast by ComputationManager when derived data is updated.
//!
//! Workers subscribe to these events to track readiness of derived data
//! computations before solving.

use super::computation::ComputationId;

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
    },

    /// All computations completed for a block.
    ///
    /// This is a convenience event - workers tracking individual computations
    /// don't need to wait for this, but it can be useful for diagnostics.
    AllComplete {
        /// Block number all computations completed for.
        block: u64,
    },
}
