//! Route-finding algorithms.
//!
//! This module defines the Algorithm trait and built-in implementations.
//! New algorithms can be added by implementing the trait.
//!
//! Algorithms are **stateful** - they own their internal derived data structures
//! (graphs, indices, caches, etc.) and update them in response to market events.
//! This design keeps algorithm-specific state encapsulated, so workers don't need
//! to know about internal data structures like graphs.

pub mod brute_force;

#[cfg(test)]
pub mod test_utils;

use std::time::Duration;

use async_trait::async_trait;
pub use brute_force::{
    scorers::{DepthAndPrice, MostLiquidScorer},
    BruteForceAlgorithm, PathScorer,
};
use tycho_simulation::tycho_core::models::Address;

use crate::{
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
    types::{solution::Order, Route},
};

pub type MostLiquidAlgorithm = BruteForceAlgorithm<MostLiquidScorer>;

/// Trait for route-finding algorithms.
///
/// Algorithms are **stateful** - they own their internal derived data structures
/// (graphs, indices, caches, etc.) and update them in response to market events.
///
/// This design:
/// - Keeps algorithm-specific derived state inside the algorithm
/// - Workers don't need to know about internal data structures
/// - Algorithms can use any internal representation they want
///
/// # Lifecycle
///
/// 1. Create the algorithm with configuration
/// 2. Call `initialize()` once with market data to build internal state
/// 3. Call `handle_event()` for each market event to keep state in sync
/// 4. Call `find_best_route()` to find routes using internal state
///
/// # Implementation Notes
///
/// - Algorithms should respect the timeout from `timeout()`
/// - They should NOT modify the market data
/// - Internal state updates happen in `initialize()` and `handle_event()`
/// - Async methods acquire read locks internally and drop them as soon as possible
#[async_trait]
pub trait Algorithm: Send + Sync {
    /// Returns the algorithm's name.
    fn name(&self) -> &str;

    /// Initializes the algorithm's internal state from market data.
    ///
    /// Called once when the worker starts. The algorithm should build
    /// whatever internal structures it needs (graph, indices, etc.).
    /// Acquires a read lock internally and drops it when done.
    async fn initialize(&mut self, market: SharedMarketDataRef);

    /// Handles multiple market events as a batch, updating internal state as needed.
    ///
    /// Called by the worker after draining pending events.
    /// Acquires a read lock once at the start, creates a minimal local copy of
    /// market data for the affected components, drops the lock, then processes events.
    ///
    /// # Arguments
    ///
    /// * `events` - Slice of market events to process
    /// * `market` - Reference to shared market data lock
    async fn handle_events(
        &mut self,
        events: &[MarketEvent],
        market: SharedMarketDataRef,
    ) -> Result<(), AlgorithmError>;

    /// Finds the best route for the given order.
    ///
    /// Uses the algorithm's internal state (not passed in).
    /// Acquires a read lock internally for state lookups and drops it ASAP.
    ///
    /// # Arguments
    ///
    /// * `market` - Reference to shared market data lock
    /// * `order` - The order to solve
    ///
    /// # Returns
    ///
    /// The best route found, or an error if no route could be found.
    async fn find_best_route(
        &self,
        market: SharedMarketDataRef,
        order: &Order,
    ) -> Result<Route, AlgorithmError>;

    /// Returns whether this algorithm supports exact-out orders.
    fn supports_exact_out(&self) -> bool;

    /// Returns the maximum number of hops to search.
    fn max_hops(&self) -> usize;

    /// Returns the timeout for route finding.
    fn timeout(&self) -> Duration;
}

/// Errors that can occur during route finding.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum AlgorithmError {
    /// Invalid algorithm configuration (programmer error).
    #[error("invalid configuration: {reason}")]
    InvalidConfiguration { reason: String },

    /// No path exists between the tokens.
    #[error("no path from {from:?} to {to:?}: {reason}")]
    NoPath { from: Address, to: Address, reason: NoPathReason },

    /// Paths exist but none have sufficient liquidity.
    #[error("insufficient liquidity on all paths")]
    InsufficientLiquidity,

    /// Route finding timed out.
    #[error("timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// Exact-out not supported by this algorithm.
    #[error("exact-out orders not supported")]
    ExactOutNotSupported,

    /// Simulation failed for a specific component.
    #[error("simulation failed for {component_id}: {error}")]
    SimulationFailed { component_id: String, error: String },

    /// Required data not found in market.
    #[error("{kind} not found: {id}")]
    DataNotFound { kind: &'static str, id: String },

    /// Other algorithm-specific error.
    #[error("{0}")]
    Other(String),
}

/// Reason why no path was found between tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoPathReason {
    /// Source token not present in the routing graph.
    SourceTokenNotInGraph,
    /// Destination token not present in the routing graph.
    DestinationTokenNotInGraph,
    /// Both tokens exist but no edges connect them within hop limits.
    NoGraphPath,
    /// Paths exist but none could be scored (e.g., missing edge weights).
    NoScorablePaths,
}

impl std::fmt::Display for NoPathReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceTokenNotInGraph => write!(f, "source token not in graph"),
            Self::DestinationTokenNotInGraph => write!(f, "destination token not in graph"),
            Self::NoGraphPath => write!(f, "no connecting path in graph"),
            Self::NoScorablePaths => write!(f, "no paths with valid scores"),
        }
    }
}
