//! Split-routing algorithm.
//!
//! For large orders, price impact makes it better to split the order across several parallel routes
//! so the marginal price stays low. `SplitAlgorithm` is the portfolio split router implemented in
//! [`split_exp`](super::split_exp): it water-fills the order across pool-disjoint paths using an
//! incremental marginal probe on a fine (256-chunk) grid, decides the active path set at coarse
//! granularity where the gas-activation gate is correct, and returns the best net of the single
//! path, a coarse split, and the refined split. It never returns less than the best single path.
//!
//! The [`split_exp`](super::split_exp) module also exposes the same machinery as the `split_incr`
//! and `split_ff` research strategies for offline benchmarking; `SplitAlgorithm` is the portfolio.

use std::time::Duration;

use super::{most_liquid::DepthAndPrice, split_exp::ExpSplitAlgorithm, Algorithm, AlgorithmConfig};
use crate::{
    derived::{computation::ComputationRequirements, SharedDerivedDataRef},
    feed::market_data::{MarketData, StateLabel},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{Order, RouteResult},
    AlgorithmError,
};

/// Routes orders by splitting them across pool-disjoint paths to minimize price impact.
///
/// A thin entry point over the portfolio split router in [`split_exp`](super::split_exp).
pub struct SplitAlgorithm {
    inner: ExpSplitAlgorithm,
}

impl SplitAlgorithm {
    /// Creates a new `SplitAlgorithm` (portfolio strategy) from an [`AlgorithmConfig`].
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self { inner: ExpSplitAlgorithm::portfolio(config)? })
    }
}

impl Algorithm for SplitAlgorithm {
    type GraphType = StableDiGraph<DepthAndPrice>;
    type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        "split"
    }

    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        self.inner
            .find_best_route(graph, market, label, derived, order)
            .await
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        self.inner.computation_requirements()
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use num_bigint::BigUint;
    use num_traits::ToPrimitive;

    use super::*;
    use crate::{
        algorithm::{
            split_test_harness::{split_metrics, two_equal_weth_usdc},
            test_utils::addr,
            MostLiquidAlgorithm,
        },
        graph::GraphManager,
        types::quote::OrderSide,
    };

    fn config() -> AlgorithmConfig {
        AlgorithmConfig::new(1, 3, Duration::from_millis(2000), None).unwrap()
    }

    /// Two equally-deep WETH/USDC pools: a large order should split across both and beat any single
    /// path.
    #[tokio::test]
    async fn split_beats_single_path_on_two_equal_pools() {
        let m = two_equal_weth_usdc(1);
        let order = Order::new(
            m.weth.clone(),
            m.usdc.clone(),
            BigUint::from(500u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        let split = SplitAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("split solves");
        let ml = MostLiquidAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("ml solves");

        let (_, path_count, split_gross) = split_metrics(&split, &m.weth, &m.usdc);
        let (_, _, ml_gross) = split_metrics(&ml, &m.weth, &m.usdc);
        assert_eq!(path_count, 2, "large order should use both pools");
        let gain = split_gross.to_f64().unwrap() / ml_gross.to_f64().unwrap();
        assert!(gain > 1.15, "expected >15% gain from splitting, got {gain:.3}x");
    }

    /// A tiny order must not lose to single-path.
    #[tokio::test]
    async fn small_order_does_not_lose_to_single_path() {
        let m = two_equal_weth_usdc(1);
        let order = Order::new(
            m.weth.clone(),
            m.usdc.clone(),
            BigUint::from(10u64).pow(15),
            OrderSide::Sell,
            addr(0xFF),
        );

        let split = SplitAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("split solves");
        let ml = MostLiquidAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("ml solves");

        let (split_net, _, _) = split_metrics(&split, &m.weth, &m.usdc);
        let (ml_net, _, _) = split_metrics(&ml, &m.weth, &m.usdc);
        assert!(
            split_net >= ml_net,
            "split must never lose to single-path: split={split_net} ml={ml_net}",
        );
    }
}
