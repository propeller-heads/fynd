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
//! - **types**: Core type definitions (`Order`, `Route`, `Swap`, `OrderSolution`)
//! - **feed**: Market data structures and event handling
//! - **worker_pool**: Multi-threaded solver pool management with algorithm registry

// Public modules
pub mod algorithm;
pub mod derived;
pub mod feed;
pub mod graph;
pub mod types;
pub mod worker_pool;

// Re-export commonly used types for convenience
pub use algorithm::{Algorithm, AlgorithmConfig, AlgorithmError, MostLiquidAlgorithm};
pub use types::{
    BlockInfo, ComponentId, Order, OrderSide, OrderSolution, OrderValidationError, Route,
    RouteResult, RouteValidationError, SingleOrderSolution, SolutionStatus, SolveError,
    SolveResult, SolveTask, Swap, TaskId,
};
pub use worker_pool::{
    pool::{WorkerPool, WorkerPoolBuilder, WorkerPoolConfig},
    registry::UnknownAlgorithmError,
    task_queue::{TaskQueue, TaskQueueConfig},
};
