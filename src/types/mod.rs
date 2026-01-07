//! Core type definitions for the Tycho Solver.
//!
//! This module contains all shared types used across the solver:
//! - `primitives`: Basic types like PoolId, ProtocolSystem, GasPrice
//! - `api`: Request/Response types for the HTTP API
//! - `solution`: Solution, Route, Swap types
//! - `internal`: Internal task and error types

pub mod primitives;
pub mod api;
pub mod solution;
pub mod internal;

// Re-export commonly used types
pub use primitives::*;
pub use api::{Order, SolutionRequest, SolutionOptions, HealthStatus};
pub use solution::{Solution, OrderSolution, Route, Swap, OrderStatus};
pub use internal::{SolveTask, SolveResult, SolveError, TaskId};
