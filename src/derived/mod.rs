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
//! // Define a computation
//! impl DerivedComputation for TokenPriceComputation {
//!     type Output = TokenPrices;
//!     const ID: ComputationId = "token_prices";
//!
//!     fn compute(&self, market: &SharedMarketData, store: &DerivedDataStore)
//!         -> Result<Self::Output, ComputationError> {
//!         // ... compute prices
//!     }
//! }
//!
//! // Create the computation manager
//! let (manager, event_rx) = ComputationManager::new(config);
//! ```

mod computation;
mod computations;
mod error;
mod store;
mod types;

pub use computation::{ComputationId, ComputationRequirements, DerivedComputation};
pub use computations::{PoolDepthComputation, SpotPriceComputation, TokenGasPriceComputation};
pub use error::ComputationError;
pub use store::DerivedDataStore;
pub use types::{
    PoolDepthKey, PoolDepths, SpotPriceKey, SpotPrices, TokenGasPriceKey, TokenGasPrices,
};
