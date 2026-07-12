//! Offline, deterministic harness for benchmarking routing algorithms.
//!
//! Production solving needs a live Tycho feed. For algorithm research we instead capture one market
//! snapshot to disk (see [`crate::feed::market_data::MarketSnapshot`]) and replay it in-process:
//! every algorithm solves the *same* frozen state, so results are reproducible and algorithms can
//! be compared fairly on output quality.
//!
//! The flow mirrors what a `SolverWorker` does, minus the async event machinery:
//! 1. Load a snapshot and wrap it in a [`MarketData`](crate::feed::market_data::MarketData) handle.
//! 2. Run the derived-data computations once ([`prepare`](crate::offline::prepare)).
//! 3. Build the algorithm's graph and edge weights once, then solve many orders
//!    ([`OfflineSolver`](crate::offline::OfflineSolver)).

use std::{path::Path, sync::Arc};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use tokio::sync::RwLock;
use tycho_simulation::tycho_common::models::Address;

use crate::{
    algorithm::Algorithm,
    derived::{ComputationManager, ComputationManagerConfig, SharedDerivedDataRef},
    feed::{
        events::{MarketEvent, MarketEventHandler},
        market_data::{MarketData, MarketDataView, MarketSnapshot, MarketState},
    },
    graph::{EdgeWeightUpdaterWithDerived, GraphManager},
    types::quote::Order,
};

/// Errors that can occur while loading or solving an offline snapshot.
#[derive(Debug, thiserror::Error)]
pub enum OfflineError {
    /// The snapshot file could not be read.
    #[error("failed to read snapshot file: {0}")]
    Io(#[from] std::io::Error),
    /// The snapshot file could not be parsed.
    #[error("failed to parse snapshot JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A derived-data computation failed while preparing the market.
    #[error("derived-data computation failed: {0}")]
    Computation(String),
}

/// Loads a market snapshot from a JSON file produced by `MarketSnapshot`.
pub fn load_snapshot(path: &Path) -> Result<MarketState, OfflineError> {
    let bytes = std::fs::read(path)?;
    let snapshot: MarketSnapshot = serde_json::from_slice(&bytes)?;
    Ok(MarketState::from_snapshot(snapshot))
}

/// Wraps a snapshot in a [`MarketData`] handle and computes derived data once.
///
/// Drives [`ComputationManager`] through a single full-recompute event so the store is populated
/// with spot prices, pool depths and token gas prices, exactly as it would be after one live block.
pub async fn prepare(
    snapshot: MarketState,
    gas_token: Address,
    max_hop: usize,
    depth_slippage_threshold: f64,
) -> Result<(MarketData, SharedDerivedDataRef), OfflineError> {
    let market = MarketData::new(Arc::new(RwLock::new(snapshot)));

    let config = ComputationManagerConfig::new()
        .with_gas_token(gas_token)
        .with_max_hop(max_hop)
        .with_depth_slippage_threshold(depth_slippage_threshold);
    let (mut manager, _rx) = ComputationManager::new(config, market.clone())
        .map_err(|e| OfflineError::Computation(e.to_string()))?;

    let topology = market.read().await.component_topology();
    let event = MarketEvent::MarketUpdated {
        added_components: topology,
        removed_components: vec![],
        updated_components: vec![],
    };
    manager
        .handle_event(&event)
        .await
        .map_err(|e| OfflineError::Computation(e.to_string()))?;

    let derived = manager.store();
    Ok((market, derived))
}

/// A reusable offline solver for a single algorithm against a frozen market.
///
/// Builds the algorithm's graph and edge weights once on construction, then [`solve`](Self::solve)
/// answers any number of orders without rebuilding.
pub struct OfflineSolver<A>
where
    A: Algorithm,
    A::GraphManager: GraphManager<A::GraphType> + EdgeWeightUpdaterWithDerived + Default,
{
    algorithm: A,
    graph_manager: A::GraphManager,
    market: MarketData,
    derived: SharedDerivedDataRef,
}

impl<A> OfflineSolver<A>
where
    A: Algorithm,
    A::GraphManager: GraphManager<A::GraphType> + EdgeWeightUpdaterWithDerived + Default,
{
    /// Builds the graph topology and edge weights for `algorithm` once.
    pub async fn new(market: MarketData, derived: SharedDerivedDataRef, algorithm: A) -> Self {
        let mut graph_manager = A::GraphManager::default();
        let topology = market.read().await.component_topology();
        graph_manager.initialize_graph(&topology);
        {
            let market_view = market.read().await;
            let derived_guard = derived.read().await;
            graph_manager.update_edge_weights_with_derived(market_view, &derived_guard);
        }
        Self { algorithm, graph_manager, market, derived }
    }

    /// Returns the algorithm's name.
    pub fn name(&self) -> &str {
        self.algorithm.name()
    }

    /// Solves a single order against the frozen market, returning quality metrics.
    pub async fn solve(&self, order: &Order) -> Result<OfflineSolution, crate::AlgorithmError> {
        let result = self
            .algorithm
            .find_best_route(
                self.graph_manager.graph(),
                self.market.clone(),
                None,
                Some(self.derived.clone()),
                order,
            )
            .await?;
        Ok(OfflineSolution::from_result(&result, order))
    }

    /// Solves a single order and returns the full route result (for route dumps / visualization).
    pub async fn solve_route(
        &self,
        order: &Order,
    ) -> Result<crate::types::RouteResult, crate::AlgorithmError> {
        self.algorithm
            .find_best_route(
                self.graph_manager.graph(),
                self.market.clone(),
                None,
                Some(self.derived.clone()),
                order,
            )
            .await
    }
}

/// Algorithm names runnable by [`run_algorithm`].
pub const AVAILABLE_ALGORITHMS: &[&str] = &[
    "most_liquid",
    "bellman_ford",
    "path_frank_wolfe",
    "split",
    "split_legacy",
    "split_bounded",
    "split_incr",
    "split_ff",
];

/// Solves every order with a named algorithm against a frozen market.
///
/// Centralizes algorithm construction so callers outside the crate (e.g. the benchmark tool) can
/// run algorithms by name without needing access to crate-private constructors. Returns one entry
/// per order: `Some(solution)` on success, `None` if the algorithm found no route.
pub async fn run_algorithm(
    market: &MarketData,
    derived: &SharedDerivedDataRef,
    algo_name: &str,
    config: crate::AlgorithmConfig,
    orders: &[Order],
) -> Result<Vec<Option<OfflineSolution>>, OfflineError> {
    use crate::algorithm::{
        path_frank_wolfe::PathFrankWolfeConfig, BellmanFordAlgorithm, ExpSplitAlgorithm,
        MostLiquidAlgorithm, PathFrankWolfeAlgorithm, SplitAlgorithm, SplitBoundedAlgorithm,
        SplitLegacyAlgorithm,
    };

    match algo_name {
        "most_liquid" => {
            let algo = MostLiquidAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "bellman_ford" => {
            let algo = BellmanFordAlgorithm::with_config(config);
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "path_frank_wolfe" => {
            let algo = PathFrankWolfeAlgorithm::new(config, PathFrankWolfeConfig::default());
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "split" => {
            let algo = SplitAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "split_legacy" => {
            let algo = SplitLegacyAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "split_bounded" => {
            let algo = SplitBoundedAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "split_incr" => {
            let algo = ExpSplitAlgorithm::refined_disjoint(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all(market, derived, algo, orders).await)
        }
        "split_ff" => {
            let algo = ExpSplitAlgorithm::fill_and_spill(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all(market, derived, algo, orders).await)
        }
        other => Err(OfflineError::Computation(format!("unknown algorithm: {other}"))),
    }
}

/// Builds an [`OfflineSolver`] for `algo` and solves every order sequentially.
async fn solve_all<A>(
    market: &MarketData,
    derived: &SharedDerivedDataRef,
    algo: A,
    orders: &[Order],
) -> Vec<Option<OfflineSolution>>
where
    A: Algorithm,
    A::GraphManager: GraphManager<A::GraphType> + EdgeWeightUpdaterWithDerived + Default,
{
    let solver = OfflineSolver::new(market.clone(), derived.clone(), algo).await;
    let mut results = Vec::with_capacity(orders.len());
    for order in orders {
        let start = std::time::Instant::now();
        let mut solution = solver.solve(order).await.ok();
        let micros = start.elapsed().as_micros() as u64;
        if let Some(s) = solution.as_mut() {
            s.solve_micros = micros;
        }
        results.push(solution);
    }
    results
}

/// A token node in a [`NormalizedRoute`], matching the route-visualization normalized schema.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NormalizedToken {
    /// Lowercase hex address, used as the unique node id.
    pub id: String,
    /// Token symbol for display.
    pub symbol: String,
    /// Token decimals for display.
    pub decimals: u8,
}

/// A swap edge in a [`NormalizedRoute`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct NormalizedSwap {
    /// Source token id (address).
    pub source: String,
    /// Target token id (address).
    pub target: String,
    /// Raw input amount (decimal string).
    pub amount_in: String,
    /// Raw output amount (decimal string).
    pub amount_out: String,
    /// Protocol system name.
    pub protocol: String,
    /// Pool / component id.
    pub pool: String,
}

/// One solved route in the route-visualization normalized schema (see the `fynd` route-viz skill).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NormalizedRoute {
    /// Human title for the chart.
    pub title: String,
    /// Chain name.
    pub chain: String,
    /// Source (input) token id.
    pub source: String,
    /// Sink (output) token id.
    pub sink: String,
    /// All token nodes referenced by the swaps.
    pub tokens: Vec<NormalizedToken>,
    /// The route's swaps as flow edges.
    pub swaps: Vec<NormalizedSwap>,
}

/// Solves every order with a named algorithm and returns each route in the normalized schema.
///
/// Mirrors [`run_algorithm`] but returns full routes instead of quality metrics, so the benchmark
/// can dump routes for the route-visualization skill. `None` where the algorithm found no route.
pub async fn run_algorithm_routes(
    market: &MarketData,
    derived: &SharedDerivedDataRef,
    algo_name: &str,
    config: crate::AlgorithmConfig,
    orders: &[Order],
    title_prefix: &str,
) -> Result<Vec<Option<NormalizedRoute>>, OfflineError> {
    use crate::algorithm::{
        path_frank_wolfe::PathFrankWolfeConfig, BellmanFordAlgorithm, ExpSplitAlgorithm,
        MostLiquidAlgorithm, PathFrankWolfeAlgorithm, SplitAlgorithm, SplitBoundedAlgorithm,
        SplitLegacyAlgorithm,
    };

    match algo_name {
        "most_liquid" => {
            let algo = MostLiquidAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "bellman_ford" => {
            let algo = BellmanFordAlgorithm::with_config(config);
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "path_frank_wolfe" => {
            let algo = PathFrankWolfeAlgorithm::new(config, PathFrankWolfeConfig::default());
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "split" => {
            let algo = SplitAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "split_legacy" => {
            let algo = SplitLegacyAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "split_bounded" => {
            let algo = SplitBoundedAlgorithm::with_config(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "split_incr" => {
            let algo = ExpSplitAlgorithm::refined_disjoint(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        "split_ff" => {
            let algo = ExpSplitAlgorithm::fill_and_spill(config)
                .map_err(|e| OfflineError::Computation(e.to_string()))?;
            Ok(solve_all_routes(market, derived, algo, orders, title_prefix).await)
        }
        other => Err(OfflineError::Computation(format!("unknown algorithm: {other}"))),
    }
}

/// Builds a solver for `algo` and returns each order's route in the normalized schema.
async fn solve_all_routes<A>(
    market: &MarketData,
    derived: &SharedDerivedDataRef,
    algo: A,
    orders: &[Order],
    title_prefix: &str,
) -> Vec<Option<NormalizedRoute>>
where
    A: Algorithm,
    A::GraphManager: GraphManager<A::GraphType> + EdgeWeightUpdaterWithDerived + Default,
{
    let solver = OfflineSolver::new(market.clone(), derived.clone(), algo).await;
    let mut routes = Vec::with_capacity(orders.len());
    for order in orders {
        let route = match solver.solve_route(order).await {
            Ok(r) => {
                let view = market.read().await;
                Some(build_normalized_route(&r, order, &view, title_prefix))
            }
            Err(_) => None,
        };
        routes.push(route);
    }
    routes
}

/// Converts a [`RouteResult`](crate::types::RouteResult) into the normalized visualization schema.
fn build_normalized_route(
    result: &crate::types::RouteResult,
    order: &Order,
    market: &MarketDataView<'_>,
    title_prefix: &str,
) -> NormalizedRoute {
    use std::collections::BTreeMap;

    let mut tokens: BTreeMap<String, NormalizedToken> = BTreeMap::new();
    let mut record = |addr: &Address| {
        let id = addr.to_string().to_lowercase();
        tokens
            .entry(id.clone())
            .or_insert_with(|| {
                let (symbol, decimals) = market
                    .get_token(addr)
                    .map(|t| (t.symbol.clone(), t.decimals as u8))
                    .unwrap_or_else(|| (short_addr(&id), 18));
                NormalizedToken { id, symbol, decimals }
            });
    };

    let mut swaps = Vec::new();
    for swap in result.route().swaps() {
        record(swap.token_in());
        record(swap.token_out());
        swaps.push(NormalizedSwap {
            source: swap
                .token_in()
                .to_string()
                .to_lowercase(),
            target: swap
                .token_out()
                .to_string()
                .to_lowercase(),
            amount_in: swap.amount_in().to_string(),
            amount_out: swap.amount_out().to_string(),
            protocol: swap.protocol().to_string(),
            pool: swap.component_id().to_string(),
        });
    }

    NormalizedRoute {
        title: title_prefix.to_string(),
        chain: "ethereum".to_string(),
        source: order
            .token_in()
            .to_string()
            .to_lowercase(),
        sink: order
            .token_out()
            .to_string()
            .to_lowercase(),
        tokens: tokens.into_values().collect(),
        swaps,
    }
}

/// Short `0xabcd…1234` label for a token with no metadata.
fn short_addr(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..6], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

/// Quality metrics for a single solved order.
///
/// `net_amount_out` is the gas-adjusted output the production router ranks on; it is the primary
/// comparison metric. `gross_amount_out` and `total_gas` are reported for context.
#[derive(Debug, Clone)]
pub struct OfflineSolution {
    /// Output after subtracting gas costs in output-token terms (can be negative). Primary metric.
    pub net_amount_out: BigInt,
    /// Total output token received, summed across terminal legs (ignores gas).
    pub gross_amount_out: BigUint,
    /// Total gas estimate across all swaps in the route.
    pub total_gas: BigUint,
    /// Number of swaps in the route.
    pub num_swaps: usize,
    /// Number of parallel paths (legs entering with the order's input token).
    pub num_paths: usize,
    /// Distinct protocols used, in first-seen order.
    pub protocols: Vec<String>,
    /// Wall-clock time to solve this order, in microseconds. Set by the offline runner.
    pub solve_micros: u64,
}

impl OfflineSolution {
    fn from_result(result: &crate::types::RouteResult, order: &Order) -> Self {
        let route = result.route();
        let token_out = order.token_out();
        let token_in = order.token_in();

        let mut gross_amount_out = BigUint::zero();
        let mut num_paths = 0usize;
        let mut protocols: Vec<String> = Vec::new();
        for swap in route.swaps() {
            if swap.token_out() == token_out {
                gross_amount_out += swap.amount_out();
            }
            if swap.token_in() == token_in {
                num_paths += 1;
            }
            let protocol = swap.protocol().to_string();
            if !protocols.contains(&protocol) {
                protocols.push(protocol);
            }
        }

        Self {
            net_amount_out: result.net_amount_out().clone(),
            gross_amount_out,
            total_gas: route.total_gas(),
            num_swaps: route.swaps().len(),
            num_paths,
            protocols,
            solve_micros: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::simulation::protocol_sim::ProtocolSim,
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::{
            test_utils::{component, token_with_decimals},
            BellmanFordAlgorithm, MostLiquidAlgorithm,
        },
        feed::market_data::MarketState,
        types::{quote::OrderSide, BlockInfo},
        AlgorithmConfig,
    };

    /// Builds a two-pool WETH/USDC market with real Uniswap-v2 states, snapshots it, and reloads it
    /// through the offline path so the test also exercises serialization.
    fn reloaded_two_pool_market() -> MarketState {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);

        // ~3000 USDC/WETH, two pools of different depth (so splitting could help later).
        let pool_a = UniswapV2State::new(
            U256::from(100u64) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(300_000u64) * U256::from(10u64).pow(U256::from(6u64)),
        );
        let pool_b = UniswapV2State::new(
            U256::from(50u64) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(150_000u64) * U256::from(10u64).pow(U256::from(6u64)),
        );

        let mut market = MarketState::new();
        market.upsert_components([
            component("pool_a", &[weth.clone(), usdc.clone()]),
            component("pool_b", &[weth.clone(), usdc.clone()]),
        ]);
        market.upsert_tokens([weth, usdc]);
        market.update_states([
            ("pool_a".to_string(), Box::new(pool_a) as Box<dyn ProtocolSim>),
            ("pool_b".to_string(), Box::new(pool_b) as Box<dyn ProtocolSim>),
        ]);
        market.update_gas_price(BlockGasPrice {
            block_number: 100,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1_000_000_000u64) },
        });
        market.update_last_updated(BlockInfo::new(100, "0xblock".to_string(), 0));

        // Round-trip through the snapshot format the harness consumes.
        let json = serde_json::to_string(&market.to_snapshot()).expect("serialize");
        let snapshot: MarketSnapshot = serde_json::from_str(&json).expect("deserialize");
        MarketState::from_snapshot(snapshot)
    }

    #[tokio::test]
    async fn offline_harness_solves_with_most_liquid_and_bellman_ford() {
        let snapshot = reloaded_two_pool_market();
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);

        let (market, derived) = prepare(snapshot, weth.address.clone(), 2, 0.01)
            .await
            .expect("prepare derived data");

        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(10u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            crate::algorithm::test_utils::addr(0xFF),
        );

        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(500), None).unwrap();

        let ml = OfflineSolver::new(
            market.clone(),
            derived.clone(),
            MostLiquidAlgorithm::with_config(config.clone()).unwrap(),
        )
        .await;
        let ml_solution = ml
            .solve(&order)
            .await
            .expect("ML solves");
        assert!(ml_solution.gross_amount_out > BigUint::from(0u64), "ML found output");
        assert!(ml_solution.num_swaps >= 1);

        let bf =
            OfflineSolver::new(market, derived, BellmanFordAlgorithm::with_config(config)).await;
        let bf_solution = bf
            .solve(&order)
            .await
            .expect("BF solves");
        assert!(bf_solution.gross_amount_out > BigUint::from(0u64), "BF found output");
    }
}
