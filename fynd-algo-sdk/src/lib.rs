//! SDK for implementing Fynd competition algorithm submissions.
//!
//! Participants implement the [`Algorithm`] trait and call [`export_algorithm!`] in their
//! `lib.rs`.  The competition harness loads the resulting shared library and invokes the
//! exported `fynd_create_spawner` symbol to obtain a [`SpawnerHandle`].
//!
//! # Quick start
//!
//! ```rust,ignore
//! use fynd_algo_sdk::prelude::*;
//!
//! pub struct MyAlgo { config: AlgorithmConfig }
//!
//! impl Algorithm for MyAlgo { /* ... */ }
//! impl AlgorithmWithConfig for MyAlgo {
//!     fn from_config(config: AlgorithmConfig) -> Self { Self { config } }
//! }
//!
//! export_algorithm!(MyAlgo, "my_algo");
//! ```

pub use fynd_core::{
    algorithm::{
        most_liquid::DepthAndPrice, Algorithm, AlgorithmConfig, AlgorithmError, NoPathReason,
    },
    derived::SharedDerivedDataRef,
    feed::market_data::SharedMarketDataRef,
    types::{Order, RouteResult},
    ComputationRequirements,
    // experimental re-exports
    EdgeWeightUpdaterWithDerived,
    PetgraphStableDiGraphManager,
    SpawnerHandle,
    StableDiGraph,
};

/// Concrete graph type alias for competition submissions.
pub type CompetitionGraph = StableDiGraph<DepthAndPrice>;
/// Concrete graph manager type alias for competition submissions.
pub type CompetitionGraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

/// Extension of [`Algorithm`] that constructs an instance from an [`AlgorithmConfig`].
///
/// Implement this alongside [`Algorithm`] so the [`export_algorithm!`] macro can create
/// per-worker instances from the pool configuration.
pub trait AlgorithmWithConfig: Algorithm {
    /// Constructs an algorithm instance from the given configuration.
    fn from_config(config: AlgorithmConfig) -> Self;
}

/// Convenience re-exports for competition submissions.
pub mod prelude {
    pub use super::{
        Algorithm, AlgorithmConfig, AlgorithmError, AlgorithmWithConfig, CompetitionGraph,
        CompetitionGraphManager, ComputationRequirements, DepthAndPrice, NoPathReason, Order,
        RouteResult, SharedDerivedDataRef, SharedMarketDataRef, SpawnerHandle,
    };
}

/// Exports an algorithm implementation from a shared library.
///
/// Generates an `extern "C"` entry point `fynd_create_spawner` that the competition harness
/// calls via `libloading`.  The returned pointer is heap-allocated and ownership transfers to
/// the caller.
///
/// # Usage
///
/// ```rust,ignore
/// use fynd_algo_sdk::prelude::*;
///
/// struct MyAlgo { /* ... */ }
/// impl Algorithm for MyAlgo { /* ... */ }
/// impl AlgorithmWithConfig for MyAlgo {
///     fn from_config(config: AlgorithmConfig) -> Self { Self { /* ... */ } }
/// }
///
/// export_algorithm!(MyAlgo, "my_algo");
/// ```
#[macro_export]
macro_rules! export_algorithm {
    ($algo_type:ty, $name:expr) => {
        #[no_mangle]
        pub extern "C" fn fynd_create_spawner() -> *mut $crate::SpawnerHandle {
            let handle = $crate::SpawnerHandle::new::<$algo_type, _>($name, |config| {
                <$algo_type as $crate::AlgorithmWithConfig>::from_config(config)
            });
            Box::into_raw(Box::new(handle))
        }
    };
}
