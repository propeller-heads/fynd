//! Derived data computation system.
//!
//! This module provides a framework for computing derived market data
//! (token prices, pool depths, spot prices, etc.) from raw market data.
//!
//! # Architecture
//!
//! - **Computations**: Implement the `DerivedComputation` trait to define new data types
//! - **Manager**: `ComputationManager` explicitly owns each computation type
//! - **Store**: `DerivedDataStore` with typed fields (no type erasure)
//! - **Events**: Broadcast notifications when computations complete
//! - **Tracker**: Per-worker readiness tracking based on algorithm requirements
//!
//! # Computation Dependencies
//!
//! Computations may depend on other computations' outputs via the `DerivedDataStore`.
//! The dependency graph must be respected when running computations:
//!
//! ```text
//!                 SpotPriceComputation
//!                    /
//!                   v
//!    PoolDepthComputation    TokenGasPriceComputation
//! ```
//!
//! - **SpotPriceComputation**: No dependencies, computes spot prices for all pools
//! - **PoolDepthComputation**: Depends on `spot_prices`
//! - **TokenGasPriceComputation**: Depends on `gas_price` (from market data); uses Bellman-Ford
//!   SPFA
//!
//! # Example
//!
//! ```ignore
//! // Create the computation manager
//! let config = ComputationManagerConfig::new(weth_address);
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
mod store;
pub(crate) mod tracker;
pub(crate) mod types;

// Only export the public API: manager, config, store, and shared reference type
pub use manager::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef};
pub use store::DerivedData;
