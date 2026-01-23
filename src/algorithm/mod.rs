//! Route-finding algorithms.
//!
//! This module defines the Algorithm trait and built-in implementations.
//! New algorithms can be added by implementing the trait.
//!
//! Algorithms are generic over their preferred graph type, allowing them to use
//! different graph crates (petgraph, custom, etc.) and leverage built-in algorithms.
//!
//! # Adding a New Algorithm
//!
//! 1. Create a new module with your algorithm implementation
//! 2. Implement the `Algorithm` trait
//! 3. Register the algorithm in `registry.rs`

pub mod most_liquid;

#[cfg(test)]
pub mod test_utils;

use std::time::Duration;

pub use most_liquid::MostLiquidAlgorithm;
use tycho_simulation::tycho_core::models::Address;

use crate::{
    feed::market_data::SharedMarketDataRef,
    graph::GraphManager,
    types::{solution::Order, Route},
};

/// Configuration for an Algorithm instance.
#[derive(Debug, Clone)]
pub(crate) struct AlgorithmConfig {
    /// Minimum hops to search (must be >= 1).
    min_hops: usize,
    /// Maximum hops to search.
    max_hops: usize,
    /// Timeout for solving.
    timeout: Duration,
}

impl AlgorithmConfig {
    /// Creates a new `AlgorithmConfig` with validation.
    ///
    /// # Errors
    ///
    /// Returns `InvalidConfiguration` if:
    /// - `min_hops == 0` (at least one hop is required)
    /// - `min_hops > max_hops`
    pub(crate) fn new(
        min_hops: usize,
        max_hops: usize,
        timeout: Duration,
    ) -> Result<Self, AlgorithmError> {
        if min_hops == 0 {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: "min_hops must be at least 1".to_string(),
            });
        }
        if min_hops > max_hops {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: format!("min_hops ({}) cannot exceed max_hops ({})", min_hops, max_hops),
            });
        }
        Ok(Self { min_hops, max_hops, timeout })
    }

    /// Returns the minimum number of hops to search.
    pub(crate) fn min_hops(&self) -> usize {
        self.min_hops
    }

    /// Returns the maximum number of hops to search.
    pub(crate) fn max_hops(&self) -> usize {
        self.max_hops
    }

    /// Returns the timeout for solving.
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Default for AlgorithmConfig {
    fn default() -> Self {
        // Default values are valid, so we can unwrap safely
        Self::new(1, 3, Duration::from_millis(100)).unwrap()
    }
}

/// Trait for route-finding algorithms.
///
/// Algorithms are generic over their preferred graph type `G`, allowing them to:
/// - Use different graph crates (petgraph, custom, etc.)
/// - Leverage built-in algorithms from graph libraries
/// - Optimize their graph representation for their specific needs
///
/// # Implementation Notes
///
/// - Algorithms should respect the timeout from `timeout()`
/// - They should use `graph` for path finding (BFS/etc)
/// - They should use `market` to read component states for simulation
/// - They should NOT modify the graph or market data
#[allow(async_fn_in_trait)] // Trait is internal; auto-trait bounds are not needed
pub(crate) trait Algorithm: Send + Sync {
    /// The graph type this algorithm uses.
    type GraphType: Send + Sync;

    /// The graph manager type for this algorithm.
    /// This allows the solver to automatically create the appropriate graph manager.
    type GraphManager: GraphManager<Self::GraphType> + Default;

    /// Returns the algorithm's name.
    fn name(&self) -> &str;

    /// Finds the best route for the given order.
    ///
    /// # Arguments
    ///
    /// * `graph` - The algorithm's preferred graph type (e.g., petgraph::Graph)
    /// * `market` - Shared reference to market data for state lookups (algorithms acquire their own
    ///   locks)
    /// * `order` - The order to solve
    ///
    /// # Returns
    ///
    /// The best route found, or an error if no route could be found.
    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
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
    #[error("{kind} not found{}", id.as_ref().map(|i| format!(": {i}")).unwrap_or_default())]
    DataNotFound { kind: &'static str, id: Option<String> },

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
