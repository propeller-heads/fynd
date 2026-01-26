//! Concrete computation implementations.
//!
//! Each computation implements the `DerivedComputation` trait from the parent module
//! to produce derived data from market data. The ComputationManager calls `compute()`
//! when relevant market events occur.

pub mod pool_depth;
pub mod spot_price;
pub mod token_gas_price;

pub use pool_depth::PoolDepthComputation;
pub use spot_price::SpotPriceComputation;
pub use token_gas_price::TokenGasPriceComputation;
