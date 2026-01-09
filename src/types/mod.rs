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

// Re-export primitive types
pub use primitives::*;

// Re-export internal types
pub use internal::{SolveError, SolveResult, SolveTask, TaskId};

// Re-export API types
pub use api::HealthStatus;

// Re-export public solution types
pub use solution::{
    // Request types
    Order, OrderKind, OrderValidationError, SolutionOptions, SolutionRequest,
    // Response types
    BlockInfo, OrderSolution, SolutionStatus, Route, RouteValidationError, Solution, Swap,
};
