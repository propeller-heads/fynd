//! Core type definitions for the Tycho Solver.
//!
//! This module contains all shared types used across the solver:
//! - [`solution`] - Public API types (requests, responses, routes, swaps)
//! - [`primitives`] - Basic types like ComponentId, ProtocolSystem, GasPrice
//! - [`api`] - HTTP API types (health check)
//! - [`internal`] - Internal task and error types

pub mod api;
pub mod constants;
pub mod internal;
pub mod primitives;
pub mod serde_helpers;
pub mod solution;

// Re-export API types
pub use api::HealthStatus;
// Re-export internal types
pub use internal::{SolveError, SolveResult, SolveTask, TaskId};
pub use primitives::*;
// Re-export public solution types
pub use solution::{
    // Response types
    BlockInfo,
    // Request types
    Order,
    OrderSide,
    OrderSolution,
    OrderValidationError,
    Route,
    RouteValidationError,
    SingleOrderSolution,
    Solution,
    SolutionOptions,
    SolutionRequest,
    SolutionStatus,
    Swap,
};
