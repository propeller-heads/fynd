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

// Only export the public API: manager, config, store, and shared reference type
pub use computation::FailedItemError;
pub use manager::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef};
pub use store::DerivedData;
