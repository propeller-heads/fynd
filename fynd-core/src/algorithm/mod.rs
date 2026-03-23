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
//! **External:** Implement the `Algorithm` trait in your own crate and plug it
//! into a [`WorkerPoolBuilder`](crate::worker_pool::pool::WorkerPoolBuilder) via
//! [`with_algorithm`](crate::worker_pool::pool::WorkerPoolBuilder::with_algorithm). No changes
//! to fynd-core required. See the `custom_algorithm` example.
//!
//! **Built-in:** To add an algorithm to the built-in registry:
//! 1. Create a new module with your algorithm implementation
//! 2. Implement the `Algorithm` trait
//! 3. Register it in `registry.rs`

pub mod bellman_ford;
pub mod bellman_ford_pricing;
pub(crate) mod bf_helpers;
pub mod most_liquid;

#[cfg(test)]
pub mod test_utils;

use std::time::Duration;

pub use bellman_ford::BellmanFordAlgorithm;
pub use most_liquid::MostLiquidAlgorithm;
use tycho_simulation::tycho_core::models::Address;

use crate::{
    derived::{computation::ComputationRequirements, SharedDerivedDataRef},
    feed::market_data::SharedMarketDataRef,
    graph::GraphManager,
    types::{quote::Order, RouteResult},
};

/// Configuration for an Algorithm instance.
#[derive(Debug, Clone)]
pub struct AlgorithmConfig {
    /// Minimum hops to search (must be >= 1).
    min_hops: usize,
    /// Maximum hops to search.
    max_hops: usize,
    /// Timeout for solving.
    timeout: Duration,
    /// Maximum number of paths to simulate. `None` means no cap.
    max_routes: Option<usize>,
    /// Enable gas-aware comparison (compares net amounts instead of gross during path selection).
    /// Currently used by Bellman-Ford; ignored by other algorithms. Defaults to true.
    gas_aware: bool,
}

impl AlgorithmConfig {
    /// Creates a new `AlgorithmConfig` with validation.
    ///
    /// # Errors
    ///
    /// Returns `InvalidConfiguration` if:
    /// - `min_hops == 0` (at least one hop is required)
    /// - `min_hops > max_hops`
    /// - `max_routes` is `Some(0)`
    pub fn new(
        min_hops: usize,
        max_hops: usize,
        timeout: Duration,
        max_routes: Option<usize>,
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
        if max_routes == Some(0) {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: "max_routes must be at least 1".to_string(),
            });
        }
        Ok(Self { min_hops, max_hops, timeout, max_routes, gas_aware: true })
    }

    /// Returns the minimum number of hops to search.
    pub fn min_hops(&self) -> usize {
        self.min_hops
    }

    /// Returns the maximum number of hops to search.
    pub fn max_hops(&self) -> usize {
        self.max_hops
    }

    /// Returns the maximum number of paths to simulate.
    pub fn max_routes(&self) -> Option<usize> {
        self.max_routes
    }

    /// Returns the timeout for solving.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns whether gas-aware comparison is enabled.
    pub fn gas_aware(&self) -> bool {
        self.gas_aware
    }

    /// Sets gas-aware comparison.
    pub fn with_gas_aware(mut self, enabled: bool) -> Self {
        self.gas_aware = enabled;
        self
    }
}

impl Default for AlgorithmConfig {
    fn default() -> Self {
        // Default values are valid, so we can unwrap safely
        Self::new(1, 3, Duration::from_millis(100), None).unwrap()
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
#[allow(async_fn_in_trait)]
pub trait Algorithm: Send + Sync {
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
    /// * `derived` - Optional shared reference to derived data (token prices, etc.)
    /// * `order` - The order to solve
    ///
    /// # Returns
    ///
    /// The best route and its gas-adjusted net output amount, or an error if no route could be
    /// found.
    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: SharedMarketDataRef,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError>;

    /// Returns the derived data computation requirements for this algorithm.
    ///
    /// Algorithms declare freshness requirements for derived data:
    /// - `require_fresh`: Data must be from the current block (same as SharedMarketData)
    /// - `allow_stale`: Data can be from any past block, as long as it exists
    ///
    /// Workers use this to determine when they can safely solve.
    ///
    /// Default implementation returns no requirements - algorithm works without
    /// any derived data.
    fn computation_requirements(&self) -> ComputationRequirements;

    /// Returns the timeout for solving.
    ///
    /// Workers use this to set the maximum time to wait for derived data
    /// before failing a solve request.
    fn timeout(&self) -> Duration;
}

/// Errors that can occur during route finding.
#[non_exhaustive]
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum AlgorithmError {
    /// Invalid algorithm configuration (programmer error).
    #[non_exhaustive]
    #[error("invalid configuration: {reason}")]
    InvalidConfiguration { reason: String },

    /// No path exists between the tokens.
    #[non_exhaustive]
    #[error("no path from {from:?} to {to:?}: {reason}")]
    NoPath { from: Address, to: Address, reason: NoPathReason },

    /// Paths exist but none have sufficient liquidity.
    #[error("insufficient liquidity on all paths")]
    InsufficientLiquidity,

    /// Route finding timed out.
    #[non_exhaustive]
    #[error("timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// Exact-out not supported by this algorithm.
    #[error("exact-out orders not supported")]
    ExactOutNotSupported,

    /// Simulation failed for a specific component.
    #[non_exhaustive]
    #[error("simulation failed for {component_id}: {error}")]
    SimulationFailed { component_id: String, error: String },

    /// Required data not found in market.
    #[non_exhaustive]
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
