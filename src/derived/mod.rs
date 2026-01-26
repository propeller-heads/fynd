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
//!                    /           \
//!                   v             v
//!    PoolDepthComputation    TokenGasPriceComputation
//! ```
//!
//! - **SpotPriceComputation**: No dependencies, computes spot prices for all pools
//! - **PoolDepthComputation**: Depends on `spot_prices`
//! - **TokenGasPriceComputation**: Depends on `spot_prices` and `gas_price` (from market data)
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

mod computation;
mod computations;
mod error;
mod manager;
mod store;
mod types;

pub use computation::{ComputationId, ComputationRequirements, DerivedComputation};
pub use computations::{PoolDepthComputation, SpotPriceComputation, TokenGasPriceComputation};
pub use error::ComputationError;
pub use manager::{ComputationManager, ComputationManagerConfig};
pub use store::DerivedData;
pub use types::{
    PoolDepthKey, PoolDepths, SpotPriceKey, SpotPrices, TokenGasPriceKey, TokenGasPrices,
};
