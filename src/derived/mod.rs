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
mod error;
mod store;
mod types;

pub use computation::{ComputationId, ComputationRequirements, DerivedComputation};
pub use error::ComputationError;
pub use store::{DerivedDataStore, PoolDepths, SpotPrices, TokenPrices};
pub use types::{PoolDepth, SpotPrice, TokenGasPrice};
