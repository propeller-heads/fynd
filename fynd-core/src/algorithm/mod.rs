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
//! 3. Register it in `worker_pool/registry.rs`
//!
//! **From outside this crate:** implement the trait and bring it in with
//! [`AlgorithmRegistry`](crate::algorithm::registry::AlgorithmRegistry); no change here is needed.

pub mod bellman_ford;
pub mod most_liquid;
pub mod path_frank_wolfe;
pub(crate) mod path_scoring;
/// Enumerating and simulating routes between two tokens.
pub mod paths;
pub mod registry;
/// What an algorithm is given to solve one order.
pub mod request;
pub(crate) mod sim_guard;
pub mod sim_meter;
/// Shared machinery for algorithms that divide an order across several paths.
pub mod split_primitives;
pub mod water_fill;

#[cfg(any(test, feature = "test-utils"))]
pub mod split_test_harness;
/// Remembers what a pool paid, so one solve asks it once per amount.
pub mod swap_cache;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

use std::time::Duration;

pub use bellman_ford::BellmanFordAlgorithm;
pub use most_liquid::MostLiquidAlgorithm;
pub use path_frank_wolfe::PathFrankWolfeAlgorithm;
pub use registry::{AlgorithmRegistry, RegisterAlgorithmError};
pub use request::SolveRequest;
use rustc_hash::FxHashSet;
use tycho_simulation::tycho_core::models::Address;
pub use water_fill::WaterFillAlgorithm;

use crate::{
    derived::computation::ComputationRequirements, graph::GraphManager, types::RouteResult,
};

/// Configuration for an Algorithm instance.
#[must_use]
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
    /// Tokens allowed as intermediate hops. `None` = no restriction (all tokens reachable).
    /// `token_in` and `token_out` for a given order are always allowed regardless.
    connector_tokens: Option<FxHashSet<Address>>,
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
        Ok(Self {
            min_hops,
            max_hops,
            timeout,
            max_routes,
            gas_aware: true,
            connector_tokens: None,
        })
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

    /// Restricts intermediate hops to the given token set.
    ///
    /// When set, only these tokens may appear between `token_in` and `token_out`
    /// in a multi-hop route. The order endpoints are always allowed regardless.
    /// Pass an empty set to disallow all intermediate hops (only 1-hop routes possible).
    pub fn with_connector_tokens(mut self, tokens: impl IntoIterator<Item = Address>) -> Self {
        self.connector_tokens = Some(tokens.into_iter().collect());
        self
    }

    /// Returns the connector token allowlist, or `None` if all tokens are permitted.
    pub fn connector_tokens(&self) -> Option<&FxHashSet<Address>> {
        self.connector_tokens.as_ref()
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

    /// Finds the best route for the order the request carries.
    ///
    /// [`SolveRequest`] holds the graph, the market, the order, the overlay to read state through,
    /// the derived data, and what the caller will accept in a route. It is `#[non_exhaustive]`, so
    /// a later addition does not break an algorithm outside this crate.
    ///
    /// Honour [`SolveRequest::filter`]. Nothing enforces it, so an algorithm that ignores it
    /// returns routes the caller asked not to have.
    ///
    /// # Returns
    ///
    /// The best route and its gas-adjusted net output amount, or an error if no route could be
    /// found.
    async fn find_best_route(
        &self,
        request: SolveRequest<'_, Self::GraphType>,
    ) -> Result<RouteResult, AlgorithmError>;

    /// Returns the derived data computation requirements for this algorithm.
    ///
    /// Algorithms declare freshness requirements for derived data:
    /// - `require_fresh`: Data must be from the current block (same as MarketState)
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
    InvalidConfiguration {
        /// Human-readable description of the invalid configuration.
        reason: String,
    },

    /// No path exists between the tokens.
    #[non_exhaustive]
    #[error("no path from {from:?} to {to:?}: {reason}")]
    NoPath {
        /// Input token address.
        from: Address,
        /// Output token address.
        to: Address,
        /// Detailed reason why no path was found.
        reason: NoPathReason,
    },

    /// Paths exist but none have sufficient liquidity.
    #[error("insufficient liquidity on all paths")]
    InsufficientLiquidity,

    /// Route finding timed out.
    #[non_exhaustive]
    #[error("timeout after {elapsed_ms}ms")]
    Timeout {
        /// Elapsed time in milliseconds when the timeout fired.
        elapsed_ms: u64,
    },

    /// Exact-out not supported by this algorithm.
    #[error("exact-out orders not supported")]
    ExactOutNotSupported,

    /// Simulation failed for a specific component.
    #[non_exhaustive]
    #[error("simulation failed for {component_id}: {error}")]
    SimulationFailed {
        /// ID of the component (liquidity pool) that failed.
        component_id: String,
        /// Underlying simulation error message.
        error: String,
    },

    /// Required data not found in market.
    #[non_exhaustive]
    #[error("{kind} not found{}", id.as_ref().map(|i| format!(": {i}")).unwrap_or_default())]
    DataNotFound {
        /// Category of the missing data (e.g. `"token"`, `"component"`).
        kind: &'static str,
        /// Optional identifier of the missing item.
        id: Option<String>,
    },

    /// Other algorithm-specific error.
    #[error("{0}")]
    Other(String),
}

impl From<crate::types::RouteValidationError> for AlgorithmError {
    fn from(error: crate::types::RouteValidationError) -> Self {
        Self::Other(error.to_string())
    }
}

/// Reason why no path was found between tokens.
#[non_exhaustive]
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
    /// The requested amount is too small to route (dust). Detection depends
    /// on scoring mode: gas-unaware scoring reports this when an explored
    /// hop's output floors to zero; gas-aware scoring reports it when an
    /// explored hop's input cannot cover that hop's gas cost. The signal
    /// latches on any explored edge, so a usable path to the destination may
    /// not have existed.
    AmountTooSmall,
}

/// Constructors for the variants that carry fields.
///
/// Those variants are `#[non_exhaustive]`, so a crate outside this one cannot build them with a
/// struct expression. This is how an algorithm implemented elsewhere reports what it found.
impl AlgorithmError {
    /// No path exists between the two tokens.
    #[must_use]
    pub fn no_path(from: Address, to: Address, reason: NoPathReason) -> Self {
        Self::NoPath { from, to, reason }
    }

    /// The search ran out of time.
    #[must_use]
    pub fn timeout(elapsed_ms: u64) -> Self {
        Self::Timeout { elapsed_ms }
    }

    /// A component refused a swap.
    #[must_use]
    pub fn simulation_failed(component_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::SimulationFailed { component_id: component_id.into(), error: error.into() }
    }

    /// The market does not hold something the algorithm needs.
    #[must_use]
    pub fn data_not_found(kind: &'static str, id: impl Into<Option<String>>) -> Self {
        Self::DataNotFound { kind, id: id.into() }
    }

    /// The algorithm was built with settings it cannot work under.
    #[must_use]
    pub fn invalid_configuration(reason: impl Into<String>) -> Self {
        Self::InvalidConfiguration { reason: reason.into() }
    }
}

impl std::fmt::Display for NoPathReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceTokenNotInGraph => write!(f, "source token not in graph"),
            Self::DestinationTokenNotInGraph => write!(f, "destination token not in graph"),
            Self::NoGraphPath => write!(f, "no connecting path in graph"),
            Self::NoScorablePaths => write!(f, "no paths with valid scores"),
            Self::AmountTooSmall => write!(f, "amount too small to route"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two same-typed arguments in a row is where an argument swap hides, and these constructors
    /// are the only way an algorithm outside this crate reports a failure.
    #[test]
    fn test_error_constructors_put_each_argument_where_its_name_says() {
        let from = Address::from(vec![0x0Au8]);
        let to = Address::from(vec![0x0Bu8]);

        match AlgorithmError::no_path(from.clone(), to.clone(), NoPathReason::NoGraphPath) {
            AlgorithmError::NoPath { from: f, to: t, reason } => {
                assert_eq!((f, t, reason), (from, to, NoPathReason::NoGraphPath));
            }
            other => panic!("expected NoPath, got {other:?}"),
        }

        match AlgorithmError::simulation_failed("pool-1", "reverted") {
            AlgorithmError::SimulationFailed { component_id, error } => {
                assert_eq!((component_id.as_str(), error.as_str()), ("pool-1", "reverted"));
            }
            other => panic!("expected SimulationFailed, got {other:?}"),
        }

        match AlgorithmError::data_not_found("token", "0x0a".to_string()) {
            AlgorithmError::DataNotFound { kind, id } => {
                assert_eq!((kind, id.as_deref()), ("token", Some("0x0a")));
            }
            other => panic!("expected DataNotFound, got {other:?}"),
        }

        assert!(matches!(AlgorithmError::timeout(42), AlgorithmError::Timeout { elapsed_ms: 42 }));
        assert!(matches!(
            AlgorithmError::invalid_configuration("bad"),
            AlgorithmError::InvalidConfiguration { .. }
        ));
    }

    #[test]
    fn test_connector_tokens_default_is_none() {
        assert!(AlgorithmConfig::default()
            .connector_tokens()
            .is_none());
    }

    #[test]
    fn test_with_connector_tokens_sets_field() {
        let addr = Address::from([0x01u8; 20]);
        let tokens: FxHashSet<Address> = FxHashSet::from_iter([addr.clone()]);
        let config = AlgorithmConfig::default().with_connector_tokens(tokens);
        let stored = config
            .connector_tokens()
            .expect("should be Some");
        assert!(stored.contains(&addr));
        assert_eq!(stored.len(), 1);
    }

    #[test]
    fn test_with_connector_tokens_empty_set() {
        let config = AlgorithmConfig::default().with_connector_tokens(FxHashSet::default());
        assert_eq!(
            config
                .connector_tokens()
                .map(|s| s.len()),
            Some(0)
        );
    }
}
