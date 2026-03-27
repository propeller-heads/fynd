//! Core type definitions for Fynd.
//!
//! This module contains all shared types used across the solver:
//! - [`types::quote`](crate::types::quote) - Public API types (requests, responses, routes, swaps)
//! - [`types::primitives`](crate::types::primitives) - Basic types like ComponentId,
//!   ProtocolSystem, GasPrice
//! - [`types::internal`](crate::types::internal) - Internal task and error types
//! - [`types::constants`](crate::types::constants) - Protocol gas costs and native token addresses

/// Protocol gas costs and native token addresses per chain.
pub mod constants;
/// Internal task and solve-error types used between the worker pool and router.
pub mod internal;
/// Primitive types: `ComponentId`, `ProtocolSystem`, `GasPrice`, `TaskId`.
pub mod primitives;
/// Public API types: `Order`, `Quote`, `Route`, `Swap`, `QuoteRequest`, etc.
pub mod quote;

// Re-export constants
pub use constants::{native_token, UnsupportedChainError};
// Re-export error types (needed for API responses)
pub use internal::{SolveError, SolveResult, SolveTask, TaskId};
pub use primitives::*;
// Re-export public quote types
pub use quote::{
    BlockInfo, ClientFeeParams, EncodingOptions, FeeBreakdown, Order, OrderQuote, OrderSide,
    OrderValidationError, PermitDetails, PermitSingle, Quote, QuoteOptions, QuoteRequest,
    QuoteStatus, Route, RouteResult, RouteValidationError, SingleOrderQuote, Swap, Transaction,
    UserTransferType,
};
