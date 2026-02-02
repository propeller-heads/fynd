//! Per-worker readiness tracking for derived data computations.
//!
//! Each worker maintains its own `ReadinessTracker` instance. The tracker is updated
//! when `DerivedDataEvent` messages are received, and queried before solving to
//! determine if required computations are ready.

use std::collections::HashSet;

use super::{
    computation::{ComputationId, ComputationRequirements},
    events::DerivedDataEvent,
};

/// Tracks which derived data computations are ready based on freshness requirements.
///
/// Workers use this to determine when they can safely acquire the `DerivedData`
/// lock and access computed data. The tracker handles two types of requirements:
///
/// - **Fresh requirements**: Must be computed for the current block
/// - **Stale requirements**: Can use data from any past block (just needs to exist)
/// TODO: Make staleness configurable by adding a max-block staleness param.
///
/// # Example
///
/// ```ignore
/// let requirements = ComputationRequirements::none()
///     .with_fresh("spot_prices")   // Must be current block
///     .with_stale("token_prices"); // Any block is fine (current design)
///
/// let mut tracker = ReadinessTracker::new(requirements);
///
/// // Process events from ComputationManager
/// tracker.handle_event(&DerivedDataEvent::NewBlock { block: 100 });
/// tracker.handle_event(&DerivedDataEvent::ComputationComplete {
///     computation_id: "spot_prices",
///     block: 100,
/// });
/// tracker.handle_event(&DerivedDataEvent::ComputationComplete {
///     computation_id: "token_prices",
///     block: 99, // stale is fine
/// });
///
/// // Check if ready to solve
/// if tracker.is_ready() {
///     // Safe to acquire DerivedData lock and solve
/// }
/// ```
#[derive(Debug)]
pub struct ReadinessTracker {
    /// Block number we're currently tracking readiness for (for fresh requirements).
    current_block: Option<u64>,
    /// Set of computation IDs that have completed for current block.
    ready_for_block: HashSet<ComputationId>,
    /// Set of computation IDs that have been computed at least once (any block).
    ever_computed: HashSet<ComputationId>,
    /// Requirements for this worker's algorithm.
    requirements: ComputationRequirements,
}

impl ReadinessTracker {
    /// Creates a new tracker with the given requirements.
    pub fn new(requirements: ComputationRequirements) -> Self {
        Self {
            current_block: None,
            ready_for_block: HashSet::new(),
            ever_computed: HashSet::new(),
            requirements,
        }
    }

    /// Creates a tracker with no requirements (always ready).
    pub fn no_requirements() -> Self {
        Self::new(ComputationRequirements::none())
    }

    /// Handles a derived data event, updating internal state.
    pub fn handle_event(&mut self, event: &DerivedDataEvent) {
        match event {
            DerivedDataEvent::NewBlock { block } => {
                self.on_new_block(*block);
            }
            DerivedDataEvent::ComputationComplete { computation_id, block } => {
                self.on_computation_complete(*computation_id, *block);
            }
            DerivedDataEvent::AllComplete { .. } => {
                // No action needed - individual ComputationComplete events
                // already updated the ready sets
            }
        }
    }

    /// Handles a new block event, clearing per-block readiness state.
    fn on_new_block(&mut self, block: u64) {
        // Only reset if this is actually a new block
        if self
            .current_block
            .map_or(true, |b| block > b)
        {
            self.current_block = Some(block);
            self.ready_for_block.clear();
            // Note: ever_computed is NOT cleared - stale data persists
        }
    }

    /// Handles a computation completion event.
    fn on_computation_complete(&mut self, computation_id: ComputationId, block: u64) {
        // Always record in ever_computed (for stale requirements)
        self.ever_computed
            .insert(computation_id);

        // For fresh requirements, check block number
        // Ignore events for blocks older than current
        if self
            .current_block
            .map_or(false, |b| block < b)
        {
            return;
        }

        // If this is a newer block, reset per-block state first
        if self
            .current_block
            .map_or(true, |b| block > b)
        {
            self.on_new_block(block);
        }

        self.ready_for_block
            .insert(computation_id);
    }

    /// Returns true if all requirements are satisfied:
    /// - All `require_fresh` computations are ready for the current block
    /// - All `allow_stale` computations have been computed at least once
    pub fn is_ready(&self) -> bool {
        let fresh_ready = self
            .requirements
            .require_fresh
            .iter()
            .all(|id| self.ready_for_block.contains(id));

        let stale_ready = self
            .requirements
            .allow_stale
            .iter()
            .all(|id| self.ever_computed.contains(id));

        fresh_ready && stale_ready
    }

    /// Returns true if the tracker has any requirements.
    pub fn has_requirements(&self) -> bool {
        self.requirements.has_requirements()
    }

    /// Returns the set of computations that are NOT yet ready.
    ///
    /// For fresh requirements: not ready for current block.
    /// For stale requirements: never computed.
    pub fn missing(&self) -> HashSet<ComputationId> {
        let missing_fresh: HashSet<_> = self
            .requirements
            .require_fresh
            .difference(&self.ready_for_block)
            .copied()
            .collect();

        let missing_stale: HashSet<_> = self
            .requirements
            .allow_stale
            .difference(&self.ever_computed)
            .copied()
            .collect();

        missing_fresh
            .union(&missing_stale)
            .copied()
            .collect()
    }

    /// Returns the current block being tracked.
    pub fn current_block(&self) -> Option<u64> {
        self.current_block
    }

    /// Returns a reference to the requirements.
    pub fn requirements(&self) -> &ComputationRequirements {
        &self.requirements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_requirements(ids: &[&'static str]) -> ComputationRequirements {
        ComputationRequirements::fresh(ids.iter().copied())
    }

    fn stale_requirements(ids: &[&'static str]) -> ComputationRequirements {
        ComputationRequirements::stale(ids.iter().copied())
    }

    #[test]
    fn new_tracker_not_ready_with_fresh_requirements() {
        let tracker = ReadinessTracker::new(fresh_requirements(&["token_prices"]));

        assert!(!tracker.is_ready());
        assert!(tracker.has_requirements());
        assert_eq!(tracker.current_block(), None);
    }

    #[test]
    fn new_tracker_ready_without_requirements() {
        let tracker = ReadinessTracker::no_requirements();

        assert!(tracker.is_ready());
        assert!(!tracker.has_requirements());
    }

    #[test]
    fn fresh_requirement_needs_current_block() {
        let mut tracker = ReadinessTracker::new(fresh_requirements(&["spot_prices"]));

        // Complete for block 100
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "spot_prices",
            block: 100,
        });
        assert!(tracker.is_ready());
        assert_eq!(tracker.current_block(), Some(100));

        // New block clears per-block ready set
        tracker.handle_event(&DerivedDataEvent::NewBlock { block: 101 });
        assert!(!tracker.is_ready()); // Not ready until computed for block 101
        assert_eq!(tracker.current_block(), Some(101));
    }

    #[test]
    fn fresh_requirement_ignores_old_blocks() {
        let mut tracker = ReadinessTracker::new(fresh_requirements(&["spot_prices"]));

        // Set current block to 100
        tracker.handle_event(&DerivedDataEvent::NewBlock { block: 100 });

        // Event for old block should NOT satisfy fresh requirement
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "spot_prices",
            block: 99,
        });

        assert!(!tracker.is_ready());
    }

    #[test]
    fn fresh_requirement_multiple_computations() {
        let mut tracker =
            ReadinessTracker::new(fresh_requirements(&["token_prices", "spot_prices"]));

        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 100,
        });
        assert!(!tracker.is_ready()); // still missing spot_prices

        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "spot_prices",
            block: 100,
        });
        assert!(tracker.is_ready());
    }

    #[test]
    fn fresh_requirement_newer_block_resets() {
        let mut tracker =
            ReadinessTracker::new(fresh_requirements(&["token_prices", "spot_prices"]));

        // Complete token_prices for block 100
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 100,
        });

        // Complete spot_prices for block 101 (newer block)
        // This should clear token_prices from per-block ready set
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "spot_prices",
            block: 101,
        });

        assert!(!tracker.is_ready()); // token_prices not ready for block 101
        assert_eq!(tracker.current_block(), Some(101));
        assert!(tracker
            .ready_for_block
            .contains(&"spot_prices"));
        assert!(!tracker
            .ready_for_block
            .contains(&"token_prices"));
    }

    #[test]
    fn new_tracker_not_ready_with_stale_requirements() {
        let tracker = ReadinessTracker::new(stale_requirements(&["token_prices"]));

        assert!(!tracker.is_ready());
        assert!(tracker.has_requirements());
    }

    #[test]
    fn stale_requirement_accepts_any_block() {
        let mut tracker = ReadinessTracker::new(stale_requirements(&["token_prices"]));

        // Complete for block 100
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 100,
        });
        assert!(tracker.is_ready());

        // New block does NOT clear stale readiness
        tracker.handle_event(&DerivedDataEvent::NewBlock { block: 101 });
        assert!(tracker.is_ready()); // Still ready - stale data persists
    }

    #[test]
    fn stale_requirement_accepts_old_blocks() {
        let mut tracker = ReadinessTracker::new(stale_requirements(&["token_prices"]));

        // Set current block to 100
        tracker.handle_event(&DerivedDataEvent::NewBlock { block: 100 });

        // Event for old block SHOULD satisfy stale requirement
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 99,
        });

        assert!(tracker.is_ready()); // Old block is fine for stale
    }

    #[test]
    fn stale_requirement_persists_across_blocks() {
        let mut tracker = ReadinessTracker::new(stale_requirements(&["token_prices"]));

        // Complete for block 100
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 100,
        });
        assert!(tracker.is_ready());

        // Move through several blocks
        for block in 101..=110 {
            tracker.handle_event(&DerivedDataEvent::NewBlock { block });
            assert!(tracker.is_ready()); // Still ready
        }

        // ever_computed should still contain token_prices
        assert!(tracker
            .ever_computed
            .contains(&"token_prices"));
    }

    #[test]
    fn mixed_fresh_and_stale_requirements() {
        let requirements = ComputationRequirements::none()
            .with_fresh("spot_prices") // Must be current block
            .with_stale("token_prices"); // Any block is fine

        let mut tracker = ReadinessTracker::new(requirements);

        // Complete token_prices for block 100
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 100,
        });
        assert!(!tracker.is_ready()); // Missing fresh spot_prices

        // Complete spot_prices for block 100
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "spot_prices",
            block: 100,
        });
        assert!(tracker.is_ready());

        // New block - spot_prices needs refresh, token_prices stays ready
        tracker.handle_event(&DerivedDataEvent::NewBlock { block: 101 });
        assert!(!tracker.is_ready()); // spot_prices not ready for 101

        // Refresh spot_prices for block 101
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "spot_prices",
            block: 101,
        });
        assert!(tracker.is_ready()); // Both satisfied again
    }

    #[test]
    fn missing_returns_unready_set() {
        let requirements = ComputationRequirements::none()
            .with_fresh("spot_prices")
            .with_stale("token_prices");

        let mut tracker = ReadinessTracker::new(requirements);

        let missing = tracker.missing();
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"spot_prices"));
        assert!(missing.contains(&"token_prices"));

        // Complete token_prices (stale)
        tracker.handle_event(&DerivedDataEvent::ComputationComplete {
            computation_id: "token_prices",
            block: 100,
        });

        let missing = tracker.missing();
        assert_eq!(missing.len(), 1);
        assert!(missing.contains(&"spot_prices"));
        assert!(!missing.contains(&"token_prices"));
    }

    #[test]
    fn all_complete_event_is_no_op() {
        let mut tracker = ReadinessTracker::new(fresh_requirements(&["token_prices"]));

        tracker.handle_event(&DerivedDataEvent::AllComplete { block: 100 });

        // Should not affect readiness
        assert!(!tracker.is_ready());
    }
}
