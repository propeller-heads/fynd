//! Core computation trait and types.

use std::collections::HashSet;

use super::{error::ComputationError, store::DerivedDataStore};
use crate::feed::market_data::SharedMarketData;

use super::error::ComputationError;
use super::store::DerivedDataStore;

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
///
/// # Example
///
/// ```ignore
/// pub struct TokenPriceComputation {
///     gas_token: Address,
/// }
///
/// impl DerivedComputation for TokenPriceComputation {
///     type Output = TokenPrices;
///     const ID: ComputationId = "token_prices";
///
///     fn compute(
///         &self,
///         market: &SharedMarketData,
///         store: &DerivedDataStore,
///     ) -> Result<Self::Output, ComputationError> {
///         // BFS from gas token through pools to compute prices
///     }
/// }
/// ```
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
    /// * `market` - Read access to raw market data (components, tokens, topology)
    /// * `store` - Read access to previously computed derived data
    ///
    /// # Returns
    ///
    /// The computed output, or an error if computation failed.
    // TODO: Support Partial Failures, including IDs for which computation failed.
    fn compute(
        &self,
        market: &SharedMarketData,
        store: &DerivedDataStore,
    ) -> Result<Self::Output, ComputationError>;
}
