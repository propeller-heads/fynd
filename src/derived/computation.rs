//! Core computation trait and types.

use std::collections::HashSet;

use async_trait::async_trait;

use super::{error::ComputationError, manager::SharedDerivedDataRef};
use crate::feed::market_data::SharedMarketDataRef;

/// Unique identifier for a computation type.
///
/// Used for event discrimination, storage keys, and readiness tracking.
pub type ComputationId = &'static str;

/// Requirements for derived data computations.
///
/// Each algorithm declares which computations it needs:
/// - `required`: Must wait for these before solving (blocks)
/// - `optional`: Best-effort, use if available (non-blocking)
#[derive(Debug, Clone, Default)]
pub struct ComputationRequirements {
    /// Computations that must complete before solving.
    pub required: HashSet<ComputationId>,
    /// Computations that are useful but not required.
    pub optional: HashSet<ComputationId>,
}

impl ComputationRequirements {
    /// Creates empty requirements (no derived data needed).
    pub fn none() -> Self {
        Self::default()
    }

    /// Creates requirements with only required computations.
    pub fn required(ids: impl IntoIterator<Item = ComputationId>) -> Self {
        Self { required: ids.into_iter().collect(), optional: HashSet::new() }
    }

    /// Builder method to add a required computation.
    pub fn with_required(mut self, id: ComputationId) -> Self {
        self.required.insert(id);
        self
    }

    /// Builder method to add an optional computation.
    pub fn with_optional(mut self, id: ComputationId) -> Self {
        self.optional.insert(id);
        self
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
///         store: &SharedDerivedDataStore,
///     ) -> Result<Self::Output, ComputationError> {
///         let market_guard = market.read().await;
///         // Use market_guard...
///         // Lock is released when guard is dropped
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
    ///
    /// # Returns
    ///
    /// The computed output, or an error if computation failed.
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
    ) -> Result<Self::Output, ComputationError>;
}
