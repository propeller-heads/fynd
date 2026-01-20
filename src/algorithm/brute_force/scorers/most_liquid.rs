//! Most Liquid scorer implementation.
//!
//! Scores paths based on spot price and liquidity depth.

use num_traits::ToPrimitive;
use tracing::trace;
use tycho_simulation::tycho_core::models::Address;

use crate::{
    algorithm::brute_force::{graph::Path, PathScorer},
    feed::market_data::SharedMarketData,
    types::ComponentId,
    AlgorithmError,
};

/// Algorithm-specific edge data for liquidity-based routing.
///
/// Used by the MostLiquid scorer to score paths based on expected output.
/// Contains the spot price and liquidity depth.
/// Note that the fee is included in the spot price already.
#[derive(Debug, Clone, Default)]
pub struct DepthAndPrice {
    /// Spot price (token_out per token_in) for this edge direction.
    pub spot_price: f64,
    /// Liquidity depth in USD (or native token terms).
    pub depth: f64,
}

impl DepthAndPrice {
    /// Creates a new DepthAndPrice with all fields set.
    pub fn new(spot_price: f64, depth: f64) -> Self {
        Self { spot_price, depth }
    }

    /// Builder method to set spot price.
    pub fn with_spot_price(mut self, spot_price: f64) -> Self {
        self.spot_price = spot_price;
        self
    }

    /// Builder method to set depth.
    pub fn with_depth(mut self, depth: f64) -> Self {
        self.depth = depth;
        self
    }
}

/// Scorer that prioritizes paths based on spot price and liquidity depth.
///
/// Scoring formula: `score = (product of all spot_prices) × min(depths)`
///
/// This accounts for:
/// - Spot price: the theoretical exchange rate along the path (not accounting for slippage)
/// - Fees: included in spot_price already
/// - Depth (inertia): minimum depth acts as a liquidity bottleneck indicator
///
/// Higher score = better path candidate. Paths through deeper pools rank higher.
#[derive(Clone, Default)]
pub struct MostLiquidScorer;

impl MostLiquidScorer {
    /// Creates a new MostLiquidScorer.
    pub fn new() -> Self {
        Self
    }
}

impl PathScorer for MostLiquidScorer {
    type EdgeData = DepthAndPrice;

    fn name(&self) -> &str {
        "most_liquid"
    }

    fn score_path(&self, path: &Path<DepthAndPrice>) -> Option<f64> {
        if path.is_empty() {
            trace!("cannot score empty path");
            return None;
        }

        let mut price = 1.0;
        let mut min_depth = f64::MAX;

        for edge in path.edge_iter() {
            let Some(data) = edge.data.as_ref() else {
                trace!(component_id = %edge.component_id, "edge missing weight data, path cannot be scored");
                return None;
            };

            price *= data.spot_price;
            min_depth = min_depth.min(data.depth);
        }

        Some(price * min_depth)
    }

    fn create_edge_data(
        &self,
        market: &SharedMarketData,
        component_id: &ComponentId,
        token_in: &Address,
        token_out: &Address,
    ) -> Result<Self::EdgeData, AlgorithmError> {
        // Get simulation state from market
        let sim = market
            .get_simulation_state(component_id)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "simulation state",
                id: component_id.clone(),
            })?;

        // Get token info for spot price calculation
        let token_in_info = market
            .get_token(token_in)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token",
                id: format!("{:?}", token_in),
            })?;
        let token_out_info = market
            .get_token(token_out)
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token",
                id: format!("{:?}", token_out),
            })?;

        let spot_price = sim
            .spot_price(token_in_info, token_out_info)
            .map_err(|e| {
                AlgorithmError::Other(format!("missing spot price for DepthAndPrice: {:?}", e))
            })?;

        let (depth_biguint, _) = sim
            .get_limits(token_in.clone(), token_out.clone())
            .map_err(|e| {
                AlgorithmError::Other(format!("missing depth for DepthAndPrice: {:?}", e))
            })?;

        let depth = depth_biguint
            .to_f64()
            .ok_or_else(|| AlgorithmError::Other("depth conversion to f64 failed".to_string()))?;

        Ok(DepthAndPrice { spot_price, depth })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::brute_force::graph::EdgeData;

    fn make_edge_data(component_id: &str, spot_price: f64, depth: f64) -> EdgeData<DepthAndPrice> {
        EdgeData::with_data(component_id.to_string(), DepthAndPrice::new(spot_price, depth))
    }

    #[test]
    fn score_path_empty_returns_none() {
        let scorer = MostLiquidScorer::new();
        let path = Path::<DepthAndPrice>::new();
        assert!(scorer.score_path(&path).is_none());
    }

    #[test]
    fn score_path_missing_data_returns_none() {
        let scorer = MostLiquidScorer::new();

        let addr_a = tycho_simulation::tycho_core::models::Address::from([0x0A; 20]);
        let addr_b = tycho_simulation::tycho_core::models::Address::from([0x0B; 20]);
        let edge_no_data: EdgeData<DepthAndPrice> = EdgeData::new("pool1".to_string());

        let mut path = Path::new();
        path.add_hop(&addr_a, &edge_no_data, &addr_b);

        assert!(scorer.score_path(&path).is_none());
    }

    #[test]
    fn score_path_calculates_correctly() {
        let scorer = MostLiquidScorer::new();

        let addr_a = tycho_simulation::tycho_core::models::Address::from([0x0A; 20]);
        let addr_b = tycho_simulation::tycho_core::models::Address::from([0x0B; 20]);
        let addr_c = tycho_simulation::tycho_core::models::Address::from([0x0C; 20]);

        // A->B: spot=2.0, depth=1000
        // B->C: spot=0.5, depth=500
        // Expected: (2.0 * 0.5) * min(1000, 500) = 1.0 * 500 = 500
        let edge_ab = make_edge_data("pool_ab", 2.0, 1000.0);
        let edge_bc = make_edge_data("pool_bc", 0.5, 500.0);

        let mut path = Path::new();
        path.add_hop(&addr_a, &edge_ab, &addr_b);
        path.add_hop(&addr_b, &edge_bc, &addr_c);

        let score = scorer.score_path(&path).unwrap();
        assert!((score - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn score_path_single_hop() {
        let scorer = MostLiquidScorer::new();

        let addr_a = tycho_simulation::tycho_core::models::Address::from([0x0A; 20]);
        let addr_b = tycho_simulation::tycho_core::models::Address::from([0x0B; 20]);

        let edge = make_edge_data("pool", 1.5, 2000.0);

        let mut path = Path::new();
        path.add_hop(&addr_a, &edge, &addr_b);

        let score = scorer.score_path(&path).unwrap();
        assert!((score - 3000.0).abs() < f64::EPSILON); // 1.5 * 2000
    }
}
