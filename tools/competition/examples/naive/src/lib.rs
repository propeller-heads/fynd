//! Naive reference submission for the Fynd algorithm competition.
//!
//! Uses the [`CompetitionGraphManager`] type aliases from the SDK and always returns
//! `AlgorithmError::InsufficientLiquidity` as a placeholder.  Replace `find_best_route`
//! with real logic to compete.

use std::time::Duration;

use fynd_algo_sdk::{export_algorithm, prelude::*};

/// Naive algorithm: placeholder that always signals insufficient liquidity.
pub struct NaiveAlgo {
    config: AlgorithmConfig,
}

#[allow(async_fn_in_trait)]
impl Algorithm for NaiveAlgo {
    type GraphType = CompetitionGraph;
    type GraphManager = CompetitionGraphManager;

    fn name(&self) -> &str {
        "naive"
    }

    async fn find_best_route(
        &self,
        _graph: &Self::GraphType,
        _market: SharedMarketDataRef,
        _derived: Option<SharedDerivedDataRef>,
        _order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        todo!("implement naive route selection")
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::default()
    }

    fn timeout(&self) -> Duration {
        self.config.timeout()
    }
}

impl AlgorithmWithConfig for NaiveAlgo {
    fn from_config(config: AlgorithmConfig) -> Self {
        Self { config }
    }
}

export_algorithm!(NaiveAlgo, "naive");
