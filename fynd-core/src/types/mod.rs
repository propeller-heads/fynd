//! Core type definitions for Fynd.
//!
//! This module contains all shared types used across the solver:
//! - [`solution`] - Public API types (requests, responses, routes, swaps)
//! - [`primitives`] - Basic types like ComponentId, ProtocolSystem, GasPrice
//! - [`internal`] - Internal task and error types
//! - [`constants`] - Protocol gas costs and native token addresses

pub mod constants;
pub mod internal;
pub mod primitives;
pub mod solution;

// Re-export constants
pub use constants::{native_token, UnsupportedChainError};
// Re-export error types (needed for API responses)
pub use internal::{SolveError, SolveResult, SolveTask, TaskId};
pub use primitives::*;
// Re-export public solution types
pub use solution::{
    BlockInfo, Order, OrderSide, OrderSolution, OrderValidationError, PriceGuardOptions, Route,
    RouteResult, RouteValidationError, SingleOrderSolution, Solution, SolutionOptions,
    SolutionRequest, SolutionStatus, Swap, Transaction,
};
