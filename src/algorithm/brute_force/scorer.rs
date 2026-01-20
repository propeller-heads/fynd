//! PathScorer trait for scoring paths in brute-force algorithms.

use tycho_simulation::tycho_core::models::Address;

use super::graph::Path;
use crate::{feed::market_data::SharedMarketData, types::ComponentId, AlgorithmError};

/// Trait for scoring paths in brute-force algorithms.
///
/// Implementations define:
/// - What data is stored on graph edges (`EdgeData`)
/// - How to calculate a score from a path's edge data
/// - How to create edge data from market data
///
/// # Type Parameters
///
/// The `EdgeData` associated type determines what data is stored on each edge
/// of the routing graph. Different scorers can use different data:
/// - `DepthAndPrice` for liquidity-based routing
/// - `VolumeAndFee` for volume-weighted routing
/// - etc.
pub trait PathScorer: Send + Sync + Clone {
    /// The edge data type this scorer uses.
    ///
    /// Must be `Clone + Default` for graph operations:
    /// - `Clone` for creating new edges with copied data
    /// - `Default` for edges that don't have data yet
    type EdgeData: Send + Sync + Clone + Default + 'static;

    /// Name of this scoring strategy (used in algorithm name and metrics).
    fn name(&self) -> &str;

    /// Scores a path based on its edge data.
    ///
    /// Returns `None` if the path cannot be scored (e.g., missing edge weights).
    /// Higher scores indicate better paths (will be simulated first).
    ///
    /// # Arguments
    ///
    /// * `path` - The path to score
    ///
    /// # Returns
    ///
    /// - `Some(score)` if the path can be scored (higher = better)
    /// - `None` if the path cannot be scored (will be filtered out)
    fn score_path(&self, path: &Path<Self::EdgeData>) -> Option<f64>;

    /// Creates edge data from market data for a specific component edge.
    ///
    /// Called when initializing or updating the graph. The scorer is responsible
    /// for looking up any data it needs from the market (tokens, simulation state, etc.).
    ///
    /// # Arguments
    ///
    /// * `market` - Shared market data containing tokens and simulation states
    /// * `component_id` - The component this edge represents
    /// * `token_in` - The input token address for this edge direction
    /// * `token_out` - The output token address for this edge direction
    ///
    /// # Returns
    ///
    /// - `Ok(data)` if edge data was created successfully
    /// - `Err(...)` if edge data could not be created (e.g., missing data)
    fn create_edge_data(
        &self,
        market: &SharedMarketData,
        component_id: &ComponentId,
        token_in: &Address,
        token_out: &Address,
    ) -> Result<Self::EdgeData, AlgorithmError>;
}
