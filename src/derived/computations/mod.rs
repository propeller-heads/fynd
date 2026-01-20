//! Concrete computation implementations.
//!
//! Each computation implements the `DerivedComputation` trait from the parent module
//! to produce derived data from market data. The ComputationManager calls `compute()`
//! when relevant market events occur.

pub mod gas_token_price;
pub mod pool_depth;
pub mod spot_price;

pub use gas_token_price::{
    GasTokenPrice, GasTokenPriceComputation, GasTokenPriceKey, GasTokenPrices,
};
pub use pool_depth::{PoolDepthComputation, PoolDepthKey, PoolDepths};
pub use spot_price::{SpotPriceComputation, SpotPriceKey, SpotPrices};
