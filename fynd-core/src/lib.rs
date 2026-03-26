//! Fynd Core - Pure solving logic for DEX routing
//!
//! This crate provides the core solving algorithms and types for finding optimal
//! swap routes across multiple DEX protocols. It contains **no HTTP or UI dependencies**,
//! making it suitable for standalone use in any application.
//!
//! # Use Cases
//!
//! - **Standalone routing**: Integrate Fynd's routing algorithms into your own application
//! - **Custom solvers**: Build specialized routing solutions without HTTP overhead
//! - **Research & testing**: Experiment with routing algorithms in isolation
//!
//! # Main Components
//!
//! - **algorithm**: Route-finding algorithms (e.g., `MostLiquidAlgorithm`)
//! - **graph**: Graph management and pathfinding utilities
//! - **derived**: Derived data computations (spot prices, pool depths, gas prices)
//! - **types**: Core type definitions (`Order`, `Route`, `Swap`, `OrderQuote`)
//! - **feed**: Market data structures and event handling
//! - **encoding**: Encodes solved routes into on-chain transactions via Tycho's router contracts
//! - **worker_pool**: Multi-threaded solver pool management with algorithm registry
//! - **worker_pool_router**: Request orchestration across multiple solver pools

// Public modules
pub mod algorithm;
pub mod derived;
pub mod encoding;
pub mod feed;
pub(crate) mod graph;
pub mod price_guard;
pub mod solver;
pub mod types;
pub mod worker_pool;
pub mod worker_pool_router;

// Re-export commonly used types for convenience
pub use algorithm::{Algorithm, AlgorithmConfig, AlgorithmError, MostLiquidAlgorithm};
// Required for implementing the Algorithm trait externally
pub use derived::computation::ComputationRequirements;
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
