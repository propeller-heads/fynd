//! Core computation trait and types.

use std::collections::HashSet;

use async_trait::async_trait;

use super::{
    error::ComputationError,
    manager::{ChangedComponents, SharedDerivedDataRef},
};
use crate::feed::market_data::SharedMarketDataRef;

/// Unique identifier for a computation type.
///
/// Used for event discrimination, storage keys, and readiness tracking.
pub type ComputationId = &'static str;

/// Error when building computation requirements.
#[derive(Debug, Clone, thiserror::Error)]
#[error("conflicting requirement: '{id}' cannot be both fresh and stale")]
pub struct RequirementConflict {
    /// The computation ID that was added with conflicting freshness.
    pub id: ComputationId,
}

/// Requirements for derived data computations.
///
/// Each algorithm declares which computations it needs and their freshness requirements:
///
/// - `require_fresh`: Data must be from the current block (same block as SharedMarketData). Workers
///   wait for these computations to complete for the current block before solving.
///
/// - `allow_stale`: Data can be from any past block, as long as it has been computed at least once.
///   Workers only check that the data exists, not that it's from the current block.
///
///
/// # Example
///
/// ```ignore
/// // Token prices don't change much block-to-block, stale is fine
/// ComputationRequirements::none()
///     .expect_stale("token_prices")?
///
/// // Spot prices must be fresh for accurate routing
/// ComputationRequirements::none()
///     .expect_fresh("spot_prices")?
/// ```
#[derive(Debug, Clone, Default)]
pub struct ComputationRequirements {
    /// Computations that must be from the current block.
    pub require_fresh: HashSet<ComputationId>,
    /// Computations that can use data from any past block.
    ///
    /// TODO: Stale data can be dangerous if stale for too long. In the future, associate staleness
    /// to a block limit might be implemented.
    pub allow_stale: HashSet<ComputationId>,
}

impl ComputationRequirements {
    /// Creates empty requirements (no derived data needed).
    pub fn none() -> Self {
        Self::default()
    }

    /// Builder method to add a computation that requires fresh data (current block).
    ///
    /// # Errors
    ///
    /// Returns `RequirementConflict` if the same ID is already in `allow_stale`.
    pub fn require_fresh(mut self, id: ComputationId) -> Result<Self, RequirementConflict> {
        if self.allow_stale.contains(&id) {
            return Err(RequirementConflict { id });
        }
        self.require_fresh.insert(id);
        Ok(self)
    }

    /// Builder method to add a computation that allows stale data (any past block).
    ///
    /// # Errors
    ///
    /// Returns `RequirementConflict` if the same ID is already in `require_fresh`.
    pub fn allow_stale(mut self, id: ComputationId) -> Result<Self, RequirementConflict> {
        if self.require_fresh.contains(&id) {
            return Err(RequirementConflict { id });
        }
        self.allow_stale.insert(id);
        Ok(self)
    }

    /// Returns true if there are any requirements.
    pub fn has_requirements(&self) -> bool {
        !self.require_fresh.is_empty() || !self.allow_stale.is_empty()
    }

    /// Returns true if the given computation is required (fresh or stale).
    pub fn is_required(&self, id: ComputationId) -> bool {
        self.require_fresh.contains(&id) || self.allow_stale.contains(&id)
    }
}

/// Trait for derived data computations.
///
/// Implement this trait to define a new type of derived data that can be
/// computed from market data.
///
/// # Design
///
/// - No `dependencies()` method - execution order is hardcoded in `ComputationManager`
/// - Typed `DerivedDataStore` - access previous results via `store.token_prices()` etc.
/// - Each computation is explicitly added to `ComputationManager`
/// - Computations receive `Arc<RwLock<>>` references and acquire locks as needed, allowing early
///   release and granular locking strategies
///
/// # Example
///
/// ```ignore
/// pub struct TokenPriceComputation {
///     gas_token: Address,
/// }
///
/// #[async_trait]
/// impl DerivedComputation for TokenPriceComputation {
///     type Output = TokenPrices;
///     const ID: ComputationId = "token_prices";
///
///     async fn compute(
///         &self,
///         market: &SharedMarketDataRef,
///         store: &SharedDerivedDataRef,
///         changed: &ChangedComponents,
///     ) -> Result<Self::Output, ComputationError> {
///         if changed.is_full_recompute {
///             // Full recompute: process all components
///         } else {
///             // Incremental: only process changed components
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait DerivedComputation: Send + Sync + 'static {
    /// The output type produced by this computation.
    ///
    /// Must be `Clone` for storage retrieval and `Send + Sync` for thread safety.
    type Output: Clone + Send + Sync + 'static;

    /// Unique identifier for this computation.
    ///
    /// Used for event discrimination, storage keys, and readiness tracking.
    const ID: ComputationId;

    /// Computes the derived data from market state.
    ///
    /// # Arguments
    ///
    /// * `market` - Reference to shared market data (computation acquires lock as needed)
    /// * `store` - Reference to derived data store (computation acquires lock as needed)
    /// * `changed` - Information about which components changed, enabling incremental computation
    ///
    /// # Returns
    ///
    /// The computed output, or an error if computation failed.
    ///
    /// # Incremental Computation
    ///
    /// Implementations should use `changed` to only recompute data affected by the changes:
    /// - `changed.is_full_recompute` - If true, recompute everything (startup/lag recovery)
    /// - `changed.added` - New components to compute
    /// - `changed.removed` - Components to remove from results
    /// - `changed.updated` - Components whose state changed
    ///
    /// # Lock Management
    ///
    /// Computations should acquire locks only when needed and release them as early
    /// as possible to minimize contention. Use `.read().await` for async lock acquisition.
    // TODO: Support Partial Failures, including IDs for which computation failed.
    async fn compute(
        &self,
        market: &SharedMarketDataRef,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Self::Output, ComputationError>;
}
