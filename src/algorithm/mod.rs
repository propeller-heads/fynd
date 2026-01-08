//! Route-finding algorithms.
//!
//! This module defines the Algorithm trait and built-in implementations.
//! New algorithms can be added by implementing the trait.
//!
//! Algorithms are generic over their preferred graph type, allowing them to use
//! different graph crates (petgraph, custom, etc.) and leverage built-in algorithms.

pub mod most_liquid;

use std::time::Duration;

pub use most_liquid::MostLiquidAlgorithm;

use crate::{
    graph::GraphManager,
    market_data::SharedMarketData,
    types::{Order, Route},
};

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
/// - They should use `market` to read pool states for simulation
/// - They should NOT modify the graph or market data
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
    /// * `market` - Reference to shared market data for state lookups
    /// * `order` - The order to solve
    ///
    /// # Returns
    ///
    /// The best route found, or an error if no route could be found.
    fn find_best_route(
        &self,
        graph: &Self::GraphType,
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
