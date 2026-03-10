//! Core type definitions for Fynd.
//!
//! This module contains all shared types used across the solver:
//! - [`quote`] - Public API types (requests, responses, routes, swaps)
//! - [`primitives`] - Basic types like ComponentId, ProtocolSystem, GasPrice
//! - [`internal`] - Internal task and error types
//! - [`constants`] - Protocol gas costs and native token addresses

pub mod constants;
pub mod internal;
pub mod primitives;
pub mod quote;

// Re-export constants
pub use constants::{native_token, UnsupportedChainError};
// Re-export error types (needed for API responses)
pub use internal::{SolveError, SolveResult, SolveTask, TaskId};
pub use primitives::*;
// Re-export public quote types
pub use quote::{
    BlockInfo, EncodingOptions, Order, OrderQuote, OrderSide, OrderValidationError, PermitDetails,
    PermitSingle, Quote, QuoteOptions, QuoteRequest, QuoteStatus, Route, RouteResult,
    RouteValidationError, SingleOrderQuote, Swap, Transaction, UserTransferType,
};
