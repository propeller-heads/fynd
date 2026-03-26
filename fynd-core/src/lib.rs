#![deny(missing_docs)]
//! Pure solving logic for the [Fynd](https://fynd.xyz) DEX router.
//!
//! This crate contains the route-finding algorithms, market-data pipeline, and encoder that
//! powers Fynd. It has **no HTTP dependencies** and can be embedded directly in any application.
//!
//! For documentation, guides, and API reference see **<https://docs.fynd.xyz/>**.
//!
//! # Use cases
//!
//! - **Standalone routing** — embed Fynd's algorithms directly without running an HTTP server.
//! - **Custom algorithms** — implement the [`Algorithm`] trait and plug in via
//!   [`FyndBuilder::with_algorithm`](solver::FyndBuilder).
//! - **HTTP server** — use the [`fynd-rpc`](https://crates.io/crates/fynd-rpc) crate, which wraps
//!   this crate with Actix Web.
//!
//! # Quick start
//!
//! See the [Fynd quickstart](https://docs.fynd.xyz/get-started/quickstart) to run a local
//! instance, or the [custom algorithm guide](https://docs.fynd.xyz/guides/custom-algorithm)
//! to implement your own routing strategy.

/// Route-finding algorithms. Includes [`MostLiquidAlgorithm`] and the
/// pluggable [`Algorithm`] trait.
pub mod algorithm;
/// Derived data computations: spot prices, pool depths, and gas prices.
pub mod derived;
/// Encodes solved routes into ABI-encoded on-chain calldata via Tycho's router contracts.
pub mod encoding;
/// Market data feed: Tycho WebSocket integration, gas price fetching, and protocol registry.
pub mod feed;
pub(crate) mod graph;
/// External price validation for quotes.
pub mod price_guard;
/// [`FyndBuilder`](solver::FyndBuilder) assembles the full pipeline and returns a
/// [`Solver`](solver::Solver).
pub mod solver;
/// Core domain types: [`Order`](types::Order), [`Route`](types::Route), [`Quote`](types::Quote),
/// etc.
pub mod types;
/// Multi-threaded solver pool management with pluggable algorithm registry.
pub mod worker_pool;
/// Request orchestration: fans out orders to all solver pools and selects the best result.
pub mod worker_pool_router;

// Re-export commonly used types for convenience
pub use algorithm::{Algorithm, AlgorithmConfig, AlgorithmError, MostLiquidAlgorithm};
// Required for implementing the Algorithm trait externally
pub use derived::computation::ComputationRequirements;
pub use price_guard::config::PriceGuardConfig;
pub use solver::{FyndBuilder, PoolConfig, Solver, SolverBuildError, SolverParts, WaitReadyError};
pub use types::{
    BlockInfo, ClientFeeParams, ComponentId, EncodingOptions, FeeBreakdown, Order, OrderQuote,
    OrderSide, OrderValidationError, PermitDetails, PermitSingle, Quote, QuoteOptions,
    QuoteRequest, QuoteStatus, Route, RouteValidationError, SingleOrderQuote, SolveError,
    SolveResult, Swap, TaskId, Transaction, UserTransferType,
};
pub use worker_pool::{
    pool::{WorkerPool, WorkerPoolBuilder, WorkerPoolConfig},
    registry::UnknownAlgorithmError,
    TaskQueueHandle,
};
pub use worker_pool_router::{config::WorkerPoolRouterConfig, SolverPoolHandle, WorkerPoolRouter};
