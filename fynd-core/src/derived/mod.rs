//! Derived data computation system.
//!
//! This module provides a framework for computing derived market data
//! (token prices, component depths, spot prices, etc.) from raw market data.
//!
//! # Architecture
//!
//! - **Computations**: Implement the `DerivedComputation` trait to define new data types
//! - **Manager**: `ComputationManager` holds a registry of computations and runs them in
//!   dependency-stage order each block
//! - **Store**: `DerivedData` keeps each computation's output in a type-keyed slot, read back
//!   through typed getters
//! - **Events**: Broadcast notifications when computations complete
//! - **Tracker**: Per-worker readiness tracking based on algorithm requirements
//!
//! # Computation Dependencies
//!
//! Each computation declares its upstream dependencies via
//! `DerivedComputation::requirements`, and the manager runs them in dependency order:
//!
//! ```text
//!                     SpotPriceComputation
//!                   /                       \
//!                  v                         v
//!    ComponentDepthComputation    TokenGasPriceComputation
//! ```
//!
//! - **SpotPriceComputation**: No dependencies, computes spot prices for all components
//! - **ComponentDepthComputation**: Depends on `spot_prices`
//! - **TokenGasPriceComputation**: Depends on `spot_prices` and `gas_price` (from market data)
//!
//! # Example
//!
//! ```ignore
//! // Create the computation manager
//! let config = ComputationManagerConfig::new().with_gas_token(weth_address);
//! let manager = ComputationManager::new(config, shared_market_data)?;
//!
//! // Get a reference to the store for workers
//! let store = manager.store();
//!
//! // Handle market events (typically from TychoFeed broadcast)
//! manager.handle_event(&event)?;
//!
//! // Workers can read derived data
//! let guard = store.read().await;
//! if let Some(prices) = guard.token_prices() {
//!     // Use prices...
//! }
//! ```

pub(crate) mod computation;
pub(crate) mod computations;
pub(crate) mod error;
pub(crate) mod events;
mod manager;
mod registry;
mod store;
pub(crate) mod tracker;
pub(crate) mod types;

// Only export the public API: the manager and its config, the store and its shared
// reference type, the data types callers exchange with it, and the reusable depth kernel
pub use computation::{ComputationId, FailedItemError};
pub use computations::component_depth::{pool_depth, PoolDepthError};
pub use manager::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef};
pub use store::DerivedData;
pub use types::{ComponentDepthKey, ComponentDepths, SpotPriceKey, SpotPrices};

/// Identifiers of the built-in computations, for
/// [`ComputationManagerConfig::with_hydrated`].
///
/// Use these rather than string literals. `POOL_DEPTHS` in particular does not match its
/// computation's name: the string is a Prometheus label that predates the rename, and an
/// unrecognised identifier is ignored rather than reported.
pub mod computation_ids {
    use super::{
        computation::DerivedComputation,
        computations::{ComponentDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
        ComputationId,
    };

    /// Spot price of every component in every token direction.
    pub const SPOT_PRICES: ComputationId = SpotPriceComputation::ID;
    /// Component depth at the configured slippage threshold. Depends on spot prices.
    pub const POOL_DEPTHS: ComputationId = ComponentDepthComputation::ID;
    /// Token price relative to the gas token. Depends on spot prices and the gas price.
    pub const TOKEN_PRICES: ComputationId = TokenGasPriceComputation::ID;
}
