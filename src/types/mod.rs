//! Core type definitions for the Tycho Solver.
//!
//! This module contains all shared types used across the solver:
//! - `primitives`: Basic types like ComponentId, ProtocolSystem, GasPrice
//! - `api`: Request/Response types for the HTTP API
//! - `solution`: Solution, Route, Swap types
//! - `internal`: Internal task and error types

pub mod api;
pub mod constants;
pub mod internal;
pub mod primitives;
pub mod solution;

// Re-export commonly used types
pub use api::{HealthStatus, Order, SolutionOptions, SolutionRequest};
pub use internal::{SolveError, SolveResult, SolveTask, TaskId};
pub use primitives::*;
pub use solution::{BlockInfo, OrderSolution, OrderStatus, Route, Solution, Swap};
