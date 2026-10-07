//! Core type definitions for Fynd.
//!
//! This module contains all shared types used across the solver:
//! - [`types::quote`] - Public API types (requests, responses, routes, swaps)
//! - [`types::primitives`] - Basic types like ComponentId, ProtocolSystem, GasPrice
//! - [`types::internal`] - Internal task and error types
//! - [`types::constants`] - Protocol gas costs and native token addresses

/// Protocol gas costs and native token addresses per chain.
pub mod constants;
/// Internal task and solve-error types used between the worker component and router.
pub mod internal;
/// Primitive types: `ComponentId`, `ProtocolSystem`, `GasPrice`, `TaskId`.
pub mod primitives;
/// Public API types: `Order`, `Quote`, `Route`, `Swap`, `QuoteRequest`, etc.
pub mod quote;
#[cfg(any(test, feature = "test-utils"))]
/// Builders for assembling quote types in tests, here and in dependent crates.
pub mod test_utils;
/// Extra ways to solve an order beside the main solve: `Variation`, `VariationQuote`.
pub mod variation;

// Re-export constants
pub use constants::{native_token, parse_chain, ParseChainError, UnsupportedChainError};
// Re-export error types (needed for API responses)
pub use internal::{RouteRejection, SolveError, SolveResult, SolveTask, TaskId};
pub use primitives::*;
// Re-export public quote types
pub use quote::{
    BlockInfo, ClientFeeParams, EncodingOptions, EncodingOptionsError, FallbackLeg, FeeBreakdown,
    Order, OrderQuote, OrderSide, OrderValidationError, PermitDetails, PermitSingle, Quote,
    QuoteOptions, QuoteRequest, QuoteStatus, Route, RouteExclusionFilter, RouteExclusions,
    RouteResult, RouteValidationError, SimulationResult, SingleOrderQuote, SolveParams,
    SurplusInfo, Swap, Transaction, UserTransferType,
};
pub(crate) use variation::VariationOutcome;
pub use variation::{
    validate_variations, Variation, VariationQuote, VariationStatus, VariationsValidationError,
    MAX_ALTERNATIVES, MAX_VARIATIONS,
};
