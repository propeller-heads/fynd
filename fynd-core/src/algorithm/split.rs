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
    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use num_traits::ToPrimitive;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::simulation::protocol_sim::ProtocolSim,
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::{
            test_utils::{addr, component, token_with_decimals},
            MostLiquidAlgorithm,
        },
        feed::market_data::MarketState,
        offline::{prepare, OfflineSolver},
        types::{quote::OrderSide, BlockInfo},
    };

    fn weth_usdc_pool(weth_reserve: u128, usdc_reserve: u128) -> UniswapV2State {
        UniswapV2State::new(
            U256::from(weth_reserve) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(usdc_reserve) * U256::from(10u64).pow(U256::from(6u64)),
        )
    }

    fn two_equal_pool_market() -> MarketState {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);

        let mut market = MarketState::new();
        market.upsert_components([
            component("pool_a", &[weth.clone(), usdc.clone()]),
            component("pool_b", &[weth.clone(), usdc.clone()]),
        ]);
        market.upsert_tokens([weth.clone(), usdc.clone()]);
        market.update_states([
            (
                "pool_a".to_string(),
                Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_b".to_string(),
                Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
            ),
        ]);
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });
        market.update_last_updated(BlockInfo::new(1, "0x01".to_string(), 0));
        market
    }

    fn config() -> AlgorithmConfig {
        AlgorithmConfig::new(1, 3, Duration::from_millis(2000), None).unwrap()
    }

    /// Two equally-deep WETH/USDC pools: a large order should split across both and beat any single
    /// path.
    #[tokio::test]
    async fn split_beats_single_path_on_two_equal_pools() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (md, derived) = prepare(two_equal_pool_market(), weth.address.clone(), 2, 0.01)
            .await
            .expect("prepare");

        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(500u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        let split = OfflineSolver::new(
            md.clone(),
            derived.clone(),
            SplitAlgorithm::with_config(config()).unwrap(),
        )
        .await
        .solve(&order)
        .await
        .expect("split solves");
        let ml =
            OfflineSolver::new(md, derived, MostLiquidAlgorithm::with_config(config()).unwrap())
                .await
                .solve(&order)
                .await
                .expect("ml solves");

        assert_eq!(split.num_paths, 2, "large order should use both pools");
        let gain = split.gross_amount_out.to_f64().unwrap() / ml.gross_amount_out.to_f64().unwrap();
        assert!(gain > 1.15, "expected >15% gain from splitting, got {gain:.3}x");
    }

    /// A tiny order must not lose to single-path.
    #[tokio::test]
    async fn small_order_does_not_lose_to_single_path() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (md, derived) = prepare(two_equal_pool_market(), weth.address.clone(), 2, 0.01)
            .await
            .expect("prepare");

        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(10u64).pow(15),
            OrderSide::Sell,
            addr(0xFF),
        );

        let split = OfflineSolver::new(
            md.clone(),
            derived.clone(),
            SplitAlgorithm::with_config(config()).unwrap(),
        )
        .await
        .solve(&order)
        .await
        .expect("split solves");
        let ml =
            OfflineSolver::new(md, derived, MostLiquidAlgorithm::with_config(config()).unwrap())
                .await
                .solve(&order)
                .await
                .expect("ml solves");

        assert!(
            split.net_amount_out >= ml.net_amount_out,
            "split must never lose to single-path: split={} ml={}",
            split.net_amount_out,
            ml.net_amount_out,
        );
    }
}
