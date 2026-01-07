//! Route-finding algorithms.
//!
//! This module defines the Algorithm trait and built-in implementations.
//! New algorithms can be added by implementing the trait.

pub mod most_liquid;

use std::time::Duration;

use crate::market_data::SharedMarketData;
use crate::route_graph::RouteGraph;
use crate::types::{Order, Route};

pub use most_liquid::MostLiquidAlgorithm;

/// Trait for route-finding algorithms.
///
/// Algorithms are stateless - they receive references to the graph and market
/// data, and return the best route they can find.
///
/// # Implementation Notes
///
/// - Algorithms should respect the timeout from `timeout()`
/// - They should use `graph` for path finding (BFS/etc)
/// - They should use `market` to read pool states for simulation
/// - They should NOT modify the graph or market data
pub trait Algorithm: Send + Sync {
    /// Returns the algorithm's name.
    fn name(&self) -> &str;

    /// Finds the best route for the given order.
    ///
    /// # Arguments
    ///
    /// * `graph` - The route graph to search (may be a solver-local copy)
    /// * `market` - Reference to shared market data for state lookups
    /// * `order` - The order to solve
    ///
    /// # Returns
    ///
    /// The best route found, or an error if no route could be found.
    fn find_best_route(
        &self,
        graph: &RouteGraph,
        market: &SharedMarketData,
        order: &Order,
    ) -> Result<Route, AlgorithmError>;

    /// Returns whether this algorithm supports exact-out orders.
    fn supports_exact_out(&self) -> bool {
        false
    }

    /// Returns the maximum number of hops to search.
    fn max_hops(&self) -> usize {
        3
    }

    /// Returns the timeout for route finding.
    fn timeout(&self) -> Duration {
        Duration::from_millis(50)
    }
}

/// Errors that can occur during route finding.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AlgorithmError {
    /// No path exists between the tokens.
    #[error("no path found between {from} and {to}")]
    NoPath { from: String, to: String },

    /// Paths exist but none have sufficient liquidity.
    #[error("insufficient liquidity on all paths")]
    InsufficientLiquidity,

    /// Route finding timed out.
    #[error("timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// Exact-out not supported by this algorithm.
    #[error("exact-out orders not supported")]
    ExactOutNotSupported,

    /// Other algorithm-specific error.
    #[error("{0}")]
    Other(String),
}

/// Registry of available algorithms.
pub struct AlgorithmRegistry {
    algorithms: Vec<Box<dyn Algorithm>>,
}

impl AlgorithmRegistry {
    /// Creates a new registry with default algorithms.
    pub fn new() -> Self {
        Self {
            algorithms: vec![Box::new(MostLiquidAlgorithm::new())],
        }
    }

    /// Registers a new algorithm.
    pub fn register(&mut self, algorithm: Box<dyn Algorithm>) {
        self.algorithms.push(algorithm);
    }

    /// Gets an algorithm by name.
    pub fn get(&self, name: &str) -> Option<&dyn Algorithm> {
        self.algorithms
            .iter()
            .find(|a| a.name() == name)
            .map(|a| a.as_ref())
    }

    /// Returns the default algorithm.
    pub fn default_algorithm(&self) -> Option<&dyn Algorithm> {
        self.algorithms.first().map(|a| a.as_ref())
    }

    /// Returns all registered algorithm names.
    pub fn names(&self) -> Vec<&str> {
        self.algorithms.iter().map(|a| a.name()).collect()
    }
}

impl Default for AlgorithmRegistry {
    fn default() -> Self {
        Self::new()
    }
}
