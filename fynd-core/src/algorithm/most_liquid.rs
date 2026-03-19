//! Most Liquid algorithm implementation.
//!
//! This algorithm finds routes by:
//! 1. Finding all edge paths up to max_hops using BFS (shorter paths first, all parallel edges)
//! 2. Scoring and sorting paths by spot price, fees, and liquidity depth
//! 3. Simulating paths with actual ProtocolSim to get accurate output (best paths first)
//! 4. Ranking by net output (output - gas cost in output token terms)
//! 5. Returning the best route with stats recorded to the tracing span

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use metrics::{counter, histogram};
use num_bigint::{BigInt, BigUint};
use num_traits::ToPrimitive;
use petgraph::prelude::EdgeRef;
use tracing::{debug, instrument, trace};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::{token::Token, Address},
};

use super::{Algorithm, AlgorithmConfig, NoPathReason};
use crate::{
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{SharedMarketData, SharedMarketDataRef},
    graph::{petgraph::StableDiGraph, Path, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult, Swap},
    AlgorithmError,
};
/// Algorithm that selects routes based on expected output after gas.
pub struct MostLiquidAlgorithm {
    min_hops: usize,
    max_hops: usize,
    timeout: Duration,
    max_routes: Option<usize>,
}

/// Algorithm-specific edge data for liquidity-based routing.
///
/// Used by the MostLiquid algorithm to score paths based on expected output.
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
    #[cfg(test)]
    pub fn new(spot_price: f64, depth: f64) -> Self {
        Self { spot_price, depth }
    }

    #[cfg(test)]
    pub fn from_protocol_sim(
        sim: &impl ProtocolSim,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<Self, AlgorithmError> {
        Ok(Self {
            spot_price: sim
                .spot_price(token_in, token_out)
                .map_err(|e| {
                    AlgorithmError::Other(format!("missing spot price for DepthAndPrice: {:?}", e))
                })?,
            depth: sim
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .map_err(|e| {
                    AlgorithmError::Other(format!("missing depth for DepthAndPrice: {:?}", e))
                })?
                .0
                .to_f64()
                .ok_or_else(|| {
                    AlgorithmError::Other("depth conversion to f64 failed".to_string())
                })?,
        })
    }
}

impl crate::graph::EdgeWeightFromSimAndDerived for DepthAndPrice {
    fn from_sim_and_derived(
        _sim: &dyn ProtocolSim,
        component_id: &ComponentId,
        token_in: &Token,
        token_out: &Token,
        derived: &crate::derived::DerivedData,
    ) -> Option<Self> {
        let key = (component_id.clone(), token_in.address.clone(), token_out.address.clone());

        // Use pre-computed spot price; fall back to zero-weight on failure.
        let spot_price = match derived
            .spot_prices()
            .and_then(|p| p.get(&key).copied())
        {
            Some(p) => p,
            None => {
                trace!(component_id = %component_id, "spot price failed, using zero weight");
                return Some(Self { spot_price: 0.0, depth: 0.0 });
            }
        };

        // Look up pre-computed depth; fall back to zero-weight on failure.
        let depth = match derived
            .pool_depths()
            .and_then(|d| d.get(&key))
        {
            Some(d) => d.to_f64()?,
            None => {
                trace!(component_id = %component_id, "pool depth failed, using zero weight");
                return Some(Self { spot_price: 0.0, depth: 0.0 });
            }
        };

        Some(Self { spot_price, depth })
    }
}

impl MostLiquidAlgorithm {
    /// Creates a new MostLiquidAlgorithm with default settings.
    pub fn new() -> Self {
        Self { min_hops: 1, max_hops: 3, timeout: Duration::from_millis(500), max_routes: None }
    }

    /// Creates a new MostLiquidAlgorithm with custom settings.
    pub fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self {
            min_hops: config.min_hops(),
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            max_routes: config.max_routes(),
        })
    }

    /// Finds all paths between two tokens using BFS directly on the graph.
    ///
    /// This is a helper method that operates on the graph without needing the graph manager.
    /// It performs BFS traversal to find all paths within the hop budget.
    ///
    /// # Errors
    ///
    /// Returns `AlgorithmError` if:
    /// - Source token is not in the graph
    /// - Destination token is not in the graph
    #[instrument(level = "debug", skip(graph))]
    fn find_paths<'a>(
        graph: &'a StableDiGraph<DepthAndPrice>,
        from: &Address,
        to: &Address,
        min_hops: usize,
        max_hops: usize,
    ) -> Result<Vec<Path<'a, DepthAndPrice>>, AlgorithmError> {
        if min_hops == 0 || min_hops > max_hops {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: format!(
                    "invalid hop configuration: min_hops={min_hops} max_hops={max_hops}",
                ),
            });
        }

        // Find source and destination nodes by address
        // TODO: this could be optimized by using a node index map in the graph manager
        let from_idx = graph
            .node_indices()
            .find(|&n| &graph[n] == from)
            .ok_or(AlgorithmError::NoPath {
                from: from.clone(),
                to: to.clone(),
                reason: NoPathReason::SourceTokenNotInGraph,
            })?;
        let to_idx = graph
            .node_indices()
            .find(|&n| &graph[n] == to)
            .ok_or(AlgorithmError::NoPath {
                from: from.clone(),
                to: to.clone(),
                reason: NoPathReason::DestinationTokenNotInGraph,
            })?;

        let mut paths = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back((from_idx, Path::new()));

        while let Some((current_node, current_path)) = queue.pop_front() {
            if current_path.len() >= max_hops {
                continue;
            }

            for edge in graph.edges(current_node) {
                let next_node = edge.target();
                let next_addr = &graph[next_node];

                // Skip paths that revisit a token already in the path.
                // Exception: when source == destination, the destination may appear at the end
                // (forming a first == last cycle, e.g. USDC → WETH → USDC). All other intermediate
                // cycles (e.g. USDC → WETH → WBTC → WETH) are not supported by Tycho execution.
                let already_visited = current_path.tokens.contains(&next_addr);
                let is_closing_circular_route = from_idx == to_idx && next_node == to_idx;
                if already_visited && !is_closing_circular_route {
                    continue;
                }

                let mut new_path = current_path.clone();
                new_path.add_hop(&graph[current_node], edge.weight(), next_addr);

                if next_node == to_idx && new_path.len() >= min_hops {
                    paths.push(new_path.clone());
                }

                queue.push_back((next_node, new_path));
            }
        }

        Ok(paths)
    }

    /// Attempts to score a path based on spot prices and minimum liquidity depth.
    ///
    /// Formula: `score = (product of all spot_price) × min(depths)`
    ///
    /// This accounts for:
    /// - Spot price: the theoretical exchange rate along the path not accounting for slippage
    /// - Fees: included in spot_price already
    /// - Depth (inertia): minimum depth acts as a liquidity bottleneck indicator
    ///
    /// Returns `None` if the path cannot be scored (empty path or missing edge weights).
    /// Paths that return `None` are filtered out of simulation.
    ///
    /// Higher score = better path candidate. Paths through deeper pools rank higher.
    fn try_score_path(path: &Path<DepthAndPrice>) -> Option<f64> {
        if path.is_empty() {
            trace!("cannot score empty path");
            return None;
        }

        let mut price = 1.0;
        let mut min_depth = f64::MAX;

        for edge in path.edge_iter() {
            let Some(data) = edge.data.as_ref() else {
                debug!(component_id = %edge.component_id, "edge missing weight data, path cannot be scored");
                return None;
            };

            price *= data.spot_price;
            min_depth = min_depth.min(data.depth);
        }

        Some(price * min_depth)
    }

    /// Simulates swaps along a path using each pool's `ProtocolSim::get_amount_out`.
    /// Tracks intermediate state changes to handle routes that revisit the same pool.
    ///
    /// Calculates `net_amount_out` by subtracting gas cost from the output amount.
    /// The result can be negative if gas cost exceeds output (e.g., inaccurate gas estimation).
    ///
    /// # Arguments
    /// * `path` - The edge path to simulate
    /// * `graph` - The graph containing edge and node data
    /// * `market` - Market data for token/component lookups and gas price
    /// * `token_prices` - Optional token prices for gas cost conversion
    /// * `amount_in` - The input amount to simulate
    #[instrument(level = "trace", skip(path, market, token_prices), fields(hop_count = path.len()))]
    pub(crate) fn simulate_path<D>(
        path: &Path<D>,
        market: &SharedMarketData,
        token_prices: Option<&TokenGasPrices>,
        amount_in: BigUint,
    ) -> Result<RouteResult, AlgorithmError> {
        let mut current_amount = amount_in.clone();
        let mut swaps = Vec::with_capacity(path.len());

        // Track state overrides for pools we've already swapped through.
        let mut state_overrides: HashMap<&ComponentId, Box<dyn ProtocolSim>> = HashMap::new();

        for (address_in, edge_data, address_out) in path.iter() {
            // Get token and component data for the simulation call
            let token_in = market
                .get_token(address_in)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: Some(format!("{:?}", address_in)),
                })?;
            let token_out = market
                .get_token(address_out)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: Some(format!("{:?}", address_out)),
                })?;

            let component_id = &edge_data.component_id;
            let component = market
                .get_component(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "component",
                    id: Some(component_id.clone()),
                })?;
            let component_state = market
                .get_simulation_state(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: Some(component_id.clone()),
                })?;

            let state = state_overrides
                .get(component_id)
                .map(Box::as_ref)
                .unwrap_or(component_state);

            // Simulate the swap
            let result = state
                .get_amount_out(current_amount.clone(), token_in, token_out)
                .map_err(|e| AlgorithmError::Other(format!("simulation error: {:?}", e)))?;

            // Record the swap
            swaps.push(Swap::new(
                component_id.clone(),
                component.protocol_system.clone(),
                token_in.address.clone(),
                token_out.address.clone(),
                current_amount.clone(),
                result.amount.clone(),
                result.gas,
                component.clone(),
                state.clone_box(),
            ));

            state_overrides.insert(component_id, result.new_state);
            current_amount = result.amount;
        }

        // Calculate net amount out (output - gas cost in output token terms)
        let route = Route::new(swaps);
        let output_amount = route
            .swaps()
            .last()
            .map(|s| s.amount_out().clone())
            .unwrap_or_else(|| BigUint::ZERO);

        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();

        let net_amount_out = if let Some(last_swap) = route.swaps().last() {
            let total_gas = route.total_gas();
            let gas_cost_wei = &total_gas * &gas_price;

            // Convert gas cost to output token terms using token prices
            let gas_cost_in_output_token: Option<BigUint> = token_prices
                .and_then(|prices| prices.get(last_swap.token_out()))
                .map(|price| {
                    // gas_cost_in_token = gas_cost_wei * numerator / denominator
                    // where numerator = tokens per ETH, denominator = 10^18 + path_gas
                    &gas_cost_wei * &price.numerator / &price.denominator
                });

            match gas_cost_in_output_token {
                Some(gas_cost) => BigInt::from(output_amount) - BigInt::from(gas_cost),
                None => {
                    // No token price available - use output amount as-is
                    // This happens if derived data hasn't been computed yet
                    BigInt::from(output_amount)
                }
            }
        } else {
            BigInt::from(output_amount)
        };

        Ok(RouteResult::new(route, net_amount_out, gas_price))
    }
}

impl Default for MostLiquidAlgorithm {
    fn default() -> Self {
        Self::new()
    }
}

impl Algorithm for MostLiquidAlgorithm {
    type GraphType = StableDiGraph<DepthAndPrice>;
    type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        "most_liquid"
    }

    // TODO: Consider adding token pair symbols to the span for easier interpretation
    #[instrument(level = "debug", skip_all, fields(order_id = %order.id()))]
    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: SharedMarketDataRef,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        let start = Instant::now();

        // Exact-out isn't supported yet
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        // Extract token prices from derived data (if available)
        let token_prices = if let Some(ref derived) = derived {
            derived
                .read()
                .await
                .token_prices()
                .cloned()
        } else {
            None
        };

        let amount_in = order.amount().clone();

        // Step 1: Find all edge paths using BFS (shorter paths first)
        let all_paths = Self::find_paths(
            graph,
            order.token_in(),
            order.token_out(),
            self.min_hops,
            self.max_hops,
        )?;

        let paths_candidates = all_paths.len();
        if paths_candidates == 0 {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        // Step 2: Score and sort all paths by estimated output (higher score = better)
        // No lock needed — scoring uses only local graph data.
        let mut scored_paths: Vec<(Path<DepthAndPrice>, f64)> = all_paths
            .into_iter()
            .filter_map(|path| {
                let score = Self::try_score_path(&path)?;
                Some((path, score))
            })
            .collect();

        scored_paths.sort_by(|(_, a_score), (_, b_score)| {
            // Flip the comparison to get descending order
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(max_routes) = self.max_routes {
            scored_paths.truncate(max_routes);
        }

        let paths_to_simulate = scored_paths.len();
        let scoring_failures = paths_candidates - paths_to_simulate;
        if paths_to_simulate == 0 {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoScorablePaths,
            });
        }

        // Step 3: Extract component IDs from all paths we'll simulate
        let component_ids: HashSet<ComponentId> = scored_paths
            .iter()
            .flat_map(|(path, _)| {
                path.edge_iter()
                    .iter()
                    .map(|e| e.component_id.clone())
            })
            .collect();

        // Step 4: Brief lock — check gas price + extract market subset for simulation
        let market = {
            let market = market.read().await;
            if market.gas_price().is_none() {
                return Err(AlgorithmError::DataNotFound { kind: "gas price", id: None });
            }
            let market_subset = market.extract_subset(&component_ids);
            drop(market);
            market_subset
        };

        let mut paths_simulated = 0usize;
        let mut simulation_failures = 0usize;

        // Step 5: Simulate all paths in score order using the local market subset
        let mut best: Option<RouteResult> = None;
        let timeout_ms = self.timeout.as_millis() as u64;

        for (edge_path, _) in scored_paths {
            // Check timeout
            let elapsed_ms = start.elapsed().as_millis() as u64;
            if elapsed_ms > timeout_ms {
                break;
            }

            let result = match Self::simulate_path(
                &edge_path,
                &market,
                token_prices.as_ref(),
                amount_in.clone(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    trace!(error = %e, "simulation failed for path");
                    simulation_failures += 1;
                    continue;
                }
            };

            // Check if this is the best result so far
            if best
                .as_ref()
                .map(|best| result.net_amount_out() > best.net_amount_out())
                .unwrap_or(true)
            {
                best = Some(result);
            }

            paths_simulated += 1;
        }

        // Log solve result
        let solve_time_ms = start.elapsed().as_millis() as u64;
        let block_number = market
            .last_updated()
            .map(|b| b.number());
        // The proportion of paths simulated to total paths that we filtered to simulate
        let coverage_pct = if paths_to_simulate == 0 {
            100.0
        } else {
            (paths_simulated as f64 / paths_to_simulate as f64) * 100.0
        };

        // Record metrics
        counter!("algorithm.scoring_failures").increment(scoring_failures as u64);
        counter!("algorithm.simulation_failures").increment(simulation_failures as u64);
        histogram!("algorithm.simulation_coverage_pct").record(coverage_pct);

        match &best {
            Some(result) => {
                let tokens = market.token_registry_ref();
                let path_desc = result.route().path_description(tokens);
                let protocols = result
                    .route()
                    .swaps()
                    .iter()
                    .map(|s| s.protocol())
                    .collect::<Vec<_>>();

                let price = amount_in
                    .to_f64()
                    .filter(|&v| v > 0.0)
                    .and_then(|amt_in| {
                        result
                            .net_amount_out()
                            .to_f64()
                            .map(|amt_out| amt_out / amt_in)
                    })
                    .unwrap_or(f64::NAN);

                debug!(
                    solve_time_ms,
                    block_number,
                    paths_candidates,
                    paths_to_simulate,
                    paths_simulated,
                    simulation_failures,
                    simulation_coverage_pct = coverage_pct,
                    components_considered = component_ids.len(),
                    tokens_considered = market.token_registry_ref().len(),
                    path = %path_desc,
                    amount_in = %amount_in,
                    net_amount_out = %result.net_amount_out(),
                    price_out_per_in = price,
                    hop_count = result.route().swaps().len(),
                    protocols = ?protocols,
                    "route found"
                );
            }
            None => {
                debug!(
                    solve_time_ms,
                    block_number,
                    paths_candidates,
                    paths_to_simulate,
                    paths_simulated,
                    simulation_failures,
                    simulation_coverage_pct = coverage_pct,
                    components_considered = component_ids.len(),
                    tokens_considered = market.token_registry_ref().len(),
                    "no viable route"
                );
            }
        }

        best.ok_or({
            if solve_time_ms > timeout_ms {
                AlgorithmError::Timeout { elapsed_ms: solve_time_ms }
            } else {
                AlgorithmError::InsufficientLiquidity
            }
        })
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        // MostLiquidAlgorithm uses token prices to convert gas costs from wei
        // to output token terms for accurate amount_out_net_gas calculation.
        //
        // Token prices are marked as `allow_stale` since they don't change much
        // block-to-block and having slightly stale prices is acceptable for
        // gas cost estimation.
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("Conflicting Computation Requirements")
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use rstest::rstest;
    use tokio::sync::RwLock;
    use tycho_simulation::{
        tycho_core::simulation::protocol_sim::Price,
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{
            addr, component,
            fixtures::{addrs, diamond_graph, linear_graph, parallel_graph},
            market_read, order, setup_market, token, MockProtocolSim, ONE_ETH,
        },
        derived::{computation::FailedItem, types::TokenGasPrices, DerivedData},
        graph::GraphManager,
        types::OrderSide,
    };

    fn wrap_market(market: SharedMarketData) -> SharedMarketDataRef {
        Arc::new(RwLock::new(market))
    }

    /// Creates a SharedDerivedDataRef with token prices set for testing.
    ///
    /// The price is set to numerator=1, denominator=1, which means:
    /// gas_cost_in_token = gas_cost_wei * 1 / 1 = gas_cost_wei
    fn setup_derived_with_token_prices(token_addresses: &[Address]) -> SharedDerivedDataRef {
        let mut token_prices: TokenGasPrices = HashMap::new();
        for addr in token_addresses {
            // Price where 1 wei of gas = 1 unit of token
            token_prices.insert(
                addr.clone(),
                Price { numerator: BigUint::from(1u64), denominator: BigUint::from(1u64) },
            );
        }

        let mut derived_data = DerivedData::new();
        derived_data.set_token_prices(token_prices, vec![], 1, true);
        Arc::new(RwLock::new(derived_data))
    }
    // ==================== try_score_path Tests ====================

    #[test]
    fn test_try_score_path_calculates_correctly() {
        let (a, b, c, _) = addrs();
        let mut m = linear_graph();

        // A->B: spot=2.0, depth=1000, fee=0.3%; B->C: spot=0.5, depth=500, fee=0.1%
        m.set_edge_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(2.0, 1000.0), false)
            .unwrap();
        m.set_edge_weight(&"bc".to_string(), &b, &c, DepthAndPrice::new(0.5, 500.0), false)
            .unwrap();

        // Use find_paths to get the 2-hop path A->B->C
        let graph = m.graph();
        let paths = MostLiquidAlgorithm::find_paths(graph, &a, &c, 2, 2).unwrap();
        assert_eq!(paths.len(), 1);
        let path = &paths[0];

        // price = 2.0 * 0.997 * 0.5 * 0.999, min_depth = 500.0
        let expected = 2.0 * 0.5 * 500.0;
        let score = MostLiquidAlgorithm::try_score_path(path).unwrap();
        assert_eq!(score, expected, "expected {expected}, got {score}");
    }

    #[test]
    fn test_try_score_path_empty_returns_none() {
        let path: Path<DepthAndPrice> = Path::new();
        assert_eq!(MostLiquidAlgorithm::try_score_path(&path), None);
    }

    #[test]
    fn test_try_score_path_missing_weight_returns_none() {
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let graph = m.graph();
        let paths = MostLiquidAlgorithm::find_paths(graph, &a, &b, 1, 1).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(MostLiquidAlgorithm::try_score_path(&paths[0]).is_none());
    }

    #[test]
    fn test_try_score_path_circular_route() {
        // Test scoring a circular path A -> B -> A
        let (a, b, _, _) = addrs();
        let mut m = linear_graph();

        // Set weights for both directions of the ab pool
        // A->B: spot=2.0, depth=1000, fee=0.3%
        // B->A: spot=0.6, depth=800, fee=0.3%
        m.set_edge_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(2.0, 1000.0), false)
            .unwrap();
        m.set_edge_weight(&"ab".to_string(), &b, &a, DepthAndPrice::new(0.6, 800.0), false)
            .unwrap();

        let graph = m.graph();
        // Find A->B->A paths (circular, 2 hops)
        let paths = MostLiquidAlgorithm::find_paths(graph, &a, &a, 2, 2).unwrap();

        // Should find at least one path
        assert_eq!(paths.len(), 1);

        // Score should be: price * min_depth
        // price = 2.0 * 0.997 * 0.6 * 0.997 = 1.1928...
        // min_depth = min(1000, 800) = 800
        // score = 1.1928 * 800 ≈ 954.3
        let score = MostLiquidAlgorithm::try_score_path(&paths[0]).unwrap();
        let expected = 2.0 * 0.6 * 800.0;
        assert_eq!(score, expected, "expected {expected}, got {score}");
    }

    fn make_mock_sim() -> MockProtocolSim {
        MockProtocolSim::new(2.0)
    }

    fn pair_key(comp: &str, b_in: u8, b_out: u8) -> (String, Address, Address) {
        (comp.to_string(), addr(b_in), addr(b_out))
    }

    fn pair_key_str(comp: &str, b_in: u8, b_out: u8) -> String {
        format!("{comp}/{}/{}", addr(b_in), addr(b_out))
    }

    #[test]
    fn test_from_sim_and_derived_failed_spot_price_returns_zero_weight() {
        let key = pair_key("pool1", 0x01, 0x02);
        let key_str = pair_key_str("pool1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        // spot price fails, pool depth not computed
        derived.set_spot_prices(
            Default::default(),
            vec![FailedItem { key: key_str, error: "sim error".to_string() }],
            10,
            true,
        );
        derived.set_pool_depths(Default::default(), vec![], 10, true);

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        let val = result.unwrap();
        assert_eq!(val.spot_price, 0.0);
        assert_eq!(val.depth, 0.0);
    }

    #[test]
    fn test_from_sim_and_derived_failed_pool_depth_returns_zero_weight() {
        let key = pair_key("pool1", 0x01, 0x02);
        let key_str = pair_key_str("pool1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        // spot price succeeds
        let mut prices = crate::derived::types::SpotPrices::default();
        prices.insert(key.clone(), 1.5);
        derived.set_spot_prices(prices, vec![], 10, true);
        // pool depth fails
        derived.set_pool_depths(
            Default::default(),
            vec![FailedItem { key: key_str, error: "depth error".to_string() }],
            10,
            true,
        );

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        let val = result.unwrap();
        assert_eq!(val.spot_price, 0.0);
        assert_eq!(val.depth, 0.0);
    }

    #[test]
    fn test_from_sim_and_derived_both_failed_returns_zero_weight() {
        let key = pair_key("pool1", 0x01, 0x02);
        let key_str = pair_key_str("pool1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        derived.set_spot_prices(
            Default::default(),
            vec![FailedItem { key: key_str.clone(), error: "spot error".to_string() }],
            10,
            true,
        );
        derived.set_pool_depths(
            Default::default(),
            vec![FailedItem { key: key_str, error: "depth error".to_string() }],
            10,
            true,
        );

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        let val = result.unwrap();
        assert_eq!(val.spot_price, 0.0);
        assert_eq!(val.depth, 0.0);
    }

    #[test]
    fn test_try_score_path_with_zero_weight_edge_returns_zero() {
        let (a, b, _, _) = addrs();
        let mut m = linear_graph();

        m.set_edge_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(0.0, 0.0), false)
            .unwrap();

        let graph = m.graph();
        let paths = MostLiquidAlgorithm::find_paths(graph, &a, &b, 1, 1).unwrap();
        assert_eq!(paths.len(), 1);
        let score = MostLiquidAlgorithm::try_score_path(&paths[0]);
        assert_eq!(score, Some(0.0));
    }

    // ==================== find_paths Tests ====================

    fn all_ids(paths: Vec<Path<'_, DepthAndPrice>>) -> HashSet<Vec<&str>> {
        paths
            .iter()
            .map(|p| {
                p.iter()
                    .map(|(_, e, _)| e.component_id.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_find_paths_linear_forward_and_reverse() {
        let (a, b, c, d) = addrs();
        let m = linear_graph();
        let g = m.graph();

        // Forward: A->B (1 hop), A->C (2 hops), A->D (3 hops)
        let p = MostLiquidAlgorithm::find_paths(g, &a, &b, 1, 1).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["ab"]]));

        let p = MostLiquidAlgorithm::find_paths(g, &a, &c, 1, 2).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["ab", "bc"]]));

        let p = MostLiquidAlgorithm::find_paths(g, &a, &d, 1, 3).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["ab", "bc", "cd"]]));

        // Reverse: D->A (bidirectional pools)
        let p = MostLiquidAlgorithm::find_paths(g, &d, &a, 1, 3).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["cd", "bc", "ab"]]));
    }

    #[test]
    fn test_find_paths_respects_hop_bounds() {
        let (a, _, c, d) = addrs();
        let m = linear_graph();
        let g = m.graph();

        // A->D needs 3 hops, max_hops=2 finds nothing
        assert!(MostLiquidAlgorithm::find_paths(g, &a, &d, 1, 2)
            .unwrap()
            .is_empty());

        // A->C is 2 hops, min_hops=3 finds nothing
        assert!(MostLiquidAlgorithm::find_paths(g, &a, &c, 3, 3)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_find_paths_parallel_pools() {
        let (a, b, c, _) = addrs();
        let m = parallel_graph();
        let g = m.graph();

        // A->B: 3 parallel pools = 3 paths
        let p = MostLiquidAlgorithm::find_paths(g, &a, &b, 1, 1).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["ab1"], vec!["ab2"], vec!["ab3"]]));

        // A->C: 3 A->B pools × 2 B->C pools = 6 paths
        let p = MostLiquidAlgorithm::find_paths(g, &a, &c, 1, 2).unwrap();
        assert_eq!(
            all_ids(p),
            HashSet::from([
                vec!["ab1", "bc1"],
                vec!["ab1", "bc2"],
                vec!["ab2", "bc1"],
                vec!["ab2", "bc2"],
                vec!["ab3", "bc1"],
                vec!["ab3", "bc2"],
            ])
        );
    }

    #[test]
    fn test_find_paths_diamond_multiple_routes() {
        let (a, _, _, d) = addrs();
        let m = diamond_graph();
        let g = m.graph();

        // A->D: two 2-hop paths
        let p = MostLiquidAlgorithm::find_paths(g, &a, &d, 1, 2).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["ab", "bd"], vec!["ac", "cd"]]));
    }

    #[test]
    fn test_find_paths_no_intermediate_cycles() {
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let g = m.graph();

        // A->B with max_hops=3: only the direct 1-hop path is valid.
        // Revisit paths like A->B->C->B or A->B->B->B are pruned because
        // they create intermediate cycles unsupported by Tycho execution
        // (only first == last cycles are allowed, i.e. from == to).
        let p = MostLiquidAlgorithm::find_paths(g, &a, &b, 1, 3).unwrap();
        assert_eq!(all_ids(p), HashSet::from([vec!["ab"]]));
    }

    #[test]
    fn test_find_paths_cyclic_same_source_dest() {
        let (a, _, _, _) = addrs();
        // Use parallel_graph with 3 A<->B pools to verify all combinations
        let m = parallel_graph();
        let g = m.graph();

        // A->A (cyclic path) with 2 hops: should find all 9 combinations (3 pools × 3 pools)
        // Note: min_hops=2 because cyclic paths require at least 2 hops
        let p = MostLiquidAlgorithm::find_paths(g, &a, &a, 2, 2).unwrap();
        assert_eq!(
            all_ids(p),
            HashSet::from([
                vec!["ab1", "ab1"],
                vec!["ab1", "ab2"],
                vec!["ab1", "ab3"],
                vec!["ab2", "ab1"],
                vec!["ab2", "ab2"],
                vec!["ab2", "ab3"],
                vec!["ab3", "ab1"],
                vec!["ab3", "ab2"],
                vec!["ab3", "ab3"],
            ])
        );
    }

    #[rstest]
    #[case::source_not_in_graph(false, true)]
    #[case::dest_not_in_graph(true, false)]
    fn test_find_paths_token_not_in_graph(#[case] from_exists: bool, #[case] to_exists: bool) {
        // Graph contains tokens A (0x0A) and B (0x0B) from linear_graph fixture
        let (a, b, _, _) = addrs();
        let non_existent = addr(0x99);
        let m = linear_graph();
        let g = m.graph();

        let from = if from_exists { a } else { non_existent.clone() };
        let to = if to_exists { b } else { non_existent };

        let result = MostLiquidAlgorithm::find_paths(g, &from, &to, 1, 3);

        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[rstest]
    #[case::min_greater_than_max(3, 1)]
    #[case::min_hops_zero(0, 1)]
    fn test_find_paths_invalid_configuration(#[case] min_hops: usize, #[case] max_hops: usize) {
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let g = m.graph();

        assert!(matches!(
            MostLiquidAlgorithm::find_paths(g, &a, &b, min_hops, max_hops)
                .err()
                .unwrap(),
            AlgorithmError::InvalidConfiguration { reason: _ }
        ));
    }

    #[test]
    fn test_find_paths_bfs_ordering() {
        // Build a graph with 1-hop, 2-hop, and 3-hop paths to E:
        //   A --[ae]--> E                          (1-hop)
        //   A --[ab]--> B --[be]--> E              (2-hop)
        //   A --[ac]--> C --[cd]--> D --[de]--> E  (3-hop)
        let (a, b, c, d) = addrs();
        let e = addr(0x0E);
        let mut m = PetgraphStableDiGraphManager::<DepthAndPrice>::new();
        let mut t = HashMap::new();
        t.insert("ae".into(), vec![a.clone(), e.clone()]);
        t.insert("ab".into(), vec![a.clone(), b.clone()]);
        t.insert("be".into(), vec![b, e.clone()]);
        t.insert("ac".into(), vec![a.clone(), c.clone()]);
        t.insert("cd".into(), vec![c, d.clone()]);
        t.insert("de".into(), vec![d, e.clone()]);
        m.initialize_graph(&t);
        let g = m.graph();

        let p = MostLiquidAlgorithm::find_paths(g, &a, &e, 1, 3).unwrap();

        // BFS guarantees paths are ordered by hop count
        assert_eq!(p.len(), 3, "Expected 3 paths total");
        assert_eq!(p[0].len(), 1, "First path should be 1-hop");
        assert_eq!(p[1].len(), 2, "Second path should be 2-hop");
        assert_eq!(p[2].len(), 3, "Third path should be 3-hop");
    }

    // ==================== simulate_path Tests ====================
    //
    // Note: These tests use MockProtocolSim which is detected as a "native" pool.
    // Ideally we should also test VM pool state override behavior (vm_state_override),
    // which shares state across all VM components. This would require a mock that
    // downcasts to EVMPoolState<PreCachedDB>, or integration tests with real VM pools.

    #[test]
    fn test_simulate_path_single_hop() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let paths = MostLiquidAlgorithm::find_paths(
            manager.graph(),
            &token_a.address,
            &token_b.address,
            1,
            1,
        )
        .unwrap();
        let path = paths.into_iter().next().unwrap();

        let result = MostLiquidAlgorithm::simulate_path(
            &path,
            &market_read(&market),
            None,
            BigUint::from(100u64),
        )
        .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(*result.route().swaps()[0].amount_in(), BigUint::from(100u64));
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(200u64)); // 100 * 2
        assert_eq!(result.route().swaps()[0].component_id(), "pool1");
    }

    #[test]
    fn test_simulate_path_multi_hop_chains_amounts() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool2", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let paths = MostLiquidAlgorithm::find_paths(
            manager.graph(),
            &token_a.address,
            &token_c.address,
            2,
            2,
        )
        .unwrap();
        let path = paths.into_iter().next().unwrap();

        let result = MostLiquidAlgorithm::simulate_path(
            &path,
            &market_read(&market),
            None,
            BigUint::from(10u64),
        )
        .unwrap();

        assert_eq!(result.route().swaps().len(), 2);
        // First hop: 10 * 2 = 20
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(20u64));
        // Second hop: 20 * 3 = 60
        assert_eq!(*result.route().swaps()[1].amount_in(), BigUint::from(20u64));
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(60u64));
    }

    #[test]
    fn test_simulate_path_same_pool_twice_uses_updated_state() {
        // Route: A -> B -> A through the same pool
        // First swap uses multiplier=2, second should use multiplier=3 (updated state)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        // A->B->A path requires min_hops=2, max_hops=2
        // Since the graph is bidirectional, we should get A->B->A path
        let paths = MostLiquidAlgorithm::find_paths(
            manager.graph(),
            &token_a.address,
            &token_a.address,
            2,
            2,
        )
        .unwrap();

        // Should only contain the A->B->A path
        assert_eq!(paths.len(), 1);
        let path = paths[0].clone();

        let result = MostLiquidAlgorithm::simulate_path(
            &path,
            &market_read(&market),
            None,
            BigUint::from(10u64),
        )
        .unwrap();

        assert_eq!(result.route().swaps().len(), 2);
        // First: 10 * 2 = 20
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(20u64));
        // Second: 20 / 3 = 6 (state updated, multiplier incremented)
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(6u64));
    }

    #[test]
    fn test_simulate_path_missing_token_returns_data_not_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, _) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);
        let market = market_read(&market);

        // Add token C to graph but not to market (A->B->C)
        let mut topology = market.component_topology();
        topology
            .insert("pool2".to_string(), vec![token_b.address.clone(), token_c.address.clone()]);
        let mut manager = PetgraphStableDiGraphManager::default();
        manager.initialize_graph(&topology);

        let graph = manager.graph();
        let paths =
            MostLiquidAlgorithm::find_paths(graph, &token_a.address, &token_c.address, 2, 2)
                .unwrap();
        let path = paths.into_iter().next().unwrap();

        let result =
            MostLiquidAlgorithm::simulate_path(&path, &market, None, BigUint::from(100u64));
        assert!(matches!(result, Err(AlgorithmError::DataNotFound { kind: "token", .. })));
    }

    #[test]
    fn test_simulate_path_missing_component_returns_data_not_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        // Remove the component but keep tokens and graph
        let mut market_write = market.try_write().unwrap();
        market_write.remove_components([&"pool1".to_string()]);
        drop(market_write);

        let graph = manager.graph();
        let paths =
            MostLiquidAlgorithm::find_paths(graph, &token_a.address, &token_b.address, 1, 1)
                .unwrap();
        let path = paths.into_iter().next().unwrap();

        let result = MostLiquidAlgorithm::simulate_path(
            &path,
            &market_read(&market),
            None,
            BigUint::from(100u64),
        );
        assert!(matches!(result, Err(AlgorithmError::DataNotFound { kind: "component", .. })));
    }

    // ==================== find_best_route Tests ====================

    #[tokio::test]
    async fn test_find_best_route_single_path() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(*result.route().swaps()[0].amount_in(), BigUint::from(ONE_ETH));
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 2));
    }

    #[tokio::test]
    async fn test_find_best_route_ranks_by_net_amount_out() {
        // Tests that route selection is based on net_amount_out (output - gas cost),
        // not just gross output. Three parallel pools with different spot_price/gas combos:
        //
        // Gas price = 100 wei/gas (set by setup_market)
        //
        // | Pool      | spot_price | gas | Output (1000 in) | Gas Cost (gas*100) | Net   |
        // |-----------|------------|-----|------------------|-------------------|-------|
        // | best      | 3          | 10  | 3000             | 1000              | 2000  |
        // | low_out   | 2          | 5   | 2000             | 500               | 1500  |
        // | high_gas  | 4          | 30  | 4000             | 3000              | 1000  |
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market(vec![
            ("best", &token_a, &token_b, MockProtocolSim::new(3.0).with_gas(10)),
            ("low_out", &token_a, &token_b, MockProtocolSim::new(2.0).with_gas(5)),
            ("high_gas", &token_a, &token_b, MockProtocolSim::new(4.0).with_gas(30)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 1000, OrderSide::Sell);

        // Set up derived data with token prices so gas can be deducted
        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algorithm
            .find_best_route(manager.graph(), market, Some(derived), &order)
            .await
            .unwrap();

        // Should select "best" pool for highest net_amount_out (2000)
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "best");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(3000u64));
        assert_eq!(result.net_amount_out(), &BigInt::from(2000)); // 3000 - 1000
    }

    #[tokio::test]
    async fn test_find_best_route_no_path_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C"); // Disconnected

        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await;
        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[tokio::test]
    async fn test_find_best_route_multi_hop() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool2", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await
            .unwrap();

        // A->B: ONE_ETH*2, B->C: (ONE_ETH*2)*3
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 2));
        assert_eq!(result.route().swaps()[0].component_id(), "pool1".to_string());
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(ONE_ETH * 2 * 3));
        assert_eq!(result.route().swaps()[1].component_id(), "pool2".to_string());
    }

    #[tokio::test]
    async fn test_find_best_route_skips_paths_without_edge_weights() {
        // Pool1 has edge weights (scoreable), Pool2 doesn't (filtered out during scoring)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Set up market with both pools using new API
        let mut market = SharedMarketData::new();
        let pool1_state = MockProtocolSim::new(2.0);
        let pool2_state = MockProtocolSim::new(3.0); // Higher multiplier but no edge weight

        let pool1_comp = component("pool1", &[token_a.clone(), token_b.clone()]);
        let pool2_comp = component("pool2", &[token_a.clone(), token_b.clone()]);

        // Set gas price (required for simulation)
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });

        // Insert components
        market.upsert_components(vec![pool1_comp, pool2_comp]);

        // Insert states
        market.update_states(vec![
            ("pool1".to_string(), Box::new(pool1_state.clone()) as Box<dyn ProtocolSim>),
            ("pool2".to_string(), Box::new(pool2_state) as Box<dyn ProtocolSim>),
        ]);

        // Insert tokens
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);

        // Initialize graph with both pools
        let mut manager = PetgraphStableDiGraphManager::default();
        manager.initialize_graph(&market.component_topology());

        // Only set edge weights for pool1, NOT pool2
        let weight = DepthAndPrice::from_protocol_sim(&pool1_state, &token_a, &token_b).unwrap();
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a.address,
                &token_b.address,
                weight,
                false,
            )
            .unwrap();

        // Use max_hops=1 to focus only on direct 1-hop paths
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let market = wrap_market(market);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await
            .unwrap();

        // Should use pool1 (only scoreable path), despite pool2 having better multiplier
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "pool1");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 2));
    }

    #[tokio::test]
    async fn test_find_best_route_no_scorable_paths() {
        // All paths exist but none have edge weights (can't be scored)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let mut market = SharedMarketData::new();
        let pool_state = MockProtocolSim::new(2.0);
        let pool_comp = component("pool1", &[token_a.clone(), token_b.clone()]);

        // Set gas price (required for simulation)
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Eip1559 {
                base_fee_per_gas: BigUint::from(1u64),
                max_priority_fee_per_gas: BigUint::from(0u64),
            },
        });

        market.upsert_components(vec![pool_comp]);
        market.update_states(vec![(
            "pool1".to_string(),
            Box::new(pool_state) as Box<dyn ProtocolSim>,
        )]);
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);

        // Initialize graph but DO NOT set any edge weights
        let mut manager = PetgraphStableDiGraphManager::default();
        manager.initialize_graph(&market.component_topology());

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let market = wrap_market(market);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::NoScorablePaths, .. })
        ));
    }

    #[tokio::test]
    async fn test_find_best_route_gas_exceeds_output_returns_negative_net() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);
        let mut market_write = market.try_write().unwrap();

        // Set a non-zero gas price so gas cost exceeds tiny output
        // gas_cost = 50_000 * (1_000_000 + 1_000_000) = 100_000_000_000 >> 2 wei output
        market_write.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Eip1559 {
                base_fee_per_gas: BigUint::from(1_000_000u64),
                max_priority_fee_per_gas: BigUint::from(1_000_000u64),
            },
        });
        drop(market_write); // Release write lock

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, 1, OrderSide::Sell); // 1 wei input -> 2 wei output

        // Set up derived data with token prices so gas can be deducted
        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        // Route should still be returned, but with negative net_amount_out
        let result = algorithm
            .find_best_route(manager.graph(), market, Some(derived), &order)
            .await
            .expect("should return route even with negative net_amount_out");

        // Verify the route has swaps
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(2u64)); // 1 * 2 = 2 wei

        // Verify it's: 2 - 200_000_000_000 = -199_999_999_998
        let expected_net = BigInt::from(2) - BigInt::from(100_000_000_000u64);
        assert_eq!(result.net_amount_out(), &expected_net);
    }

    #[tokio::test]
    async fn test_find_best_route_insufficient_liquidity() {
        // Pool has limited liquidity (1000 wei) but we try to swap ONE_ETH
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_liquidity(1000),
        )]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell); // More than 1000 wei liquidity

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await;
        assert!(matches!(result, Err(AlgorithmError::InsufficientLiquidity)));
    }

    #[tokio::test]
    async fn test_find_best_route_missing_gas_price_returns_error() {
        // Test that missing gas price returns DataNotFound error, not InsufficientLiquidity
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let mut market = SharedMarketData::new();
        let pool_state = MockProtocolSim::new(2.0);
        let pool_comp = component("pool1", &[token_a.clone(), token_b.clone()]);

        // DO NOT set gas price - this is what we're testing
        market.upsert_components(vec![pool_comp]);
        market.update_states(vec![(
            "pool1".to_string(),
            Box::new(pool_state.clone()) as Box<dyn ProtocolSim>,
        )]);
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);

        // Initialize graph and set edge weights
        let mut manager = PetgraphStableDiGraphManager::default();
        manager.initialize_graph(&market.component_topology());
        let weight = DepthAndPrice::from_protocol_sim(&pool_state, &token_a, &token_b).unwrap();
        manager
            .set_edge_weight(
                &"pool1".to_string(),
                &token_a.address,
                &token_b.address,
                weight,
                false,
            )
            .unwrap();

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let market = wrap_market(market);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await;

        // Should get DataNotFound for gas price, not InsufficientLiquidity
        assert!(matches!(result, Err(AlgorithmError::DataNotFound { kind: "gas price", .. })));
    }

    #[tokio::test]
    async fn test_find_best_route_circular_arbitrage() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // MockProtocolSim::get_amount_out multiplies by spot_price when token_in < token_out.
        // After the first swap, spot_price increments to 3.
        let (market, manager) =
            setup_market(vec![("pool1", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        // Use min_hops=2 to require at least 2 hops (circular)
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();

        // Order: swap A for A (circular)
        let order = order(&token_a, &token_a, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await
            .unwrap();

        // Should have 2 swaps forming a circle
        assert_eq!(result.route().swaps().len(), 2, "Should have 2 swaps for circular route");

        // First swap: A -> B (100 * 2 = 200)
        assert_eq!(*result.route().swaps()[0].token_in(), token_a.address);
        assert_eq!(*result.route().swaps()[0].token_out(), token_b.address);
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(200u64));

        // Second swap: B -> A (200 / 3 = 66, spot_price incremented to 3)
        assert_eq!(*result.route().swaps()[1].token_in(), token_b.address);
        assert_eq!(*result.route().swaps()[1].token_out(), token_a.address);
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(66u64));

        // Verify the route starts and ends with the same token
        assert_eq!(result.route().swaps()[0].token_in(), result.route().swaps()[1].token_out());
    }

    #[tokio::test]
    async fn test_find_best_route_respects_min_hops() {
        // Setup: A->B (1-hop) and A->C->B (2-hop)
        // With min_hops=2, should only return the 2-hop path
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(10.0)), /* Direct: 1-hop, high
                                                                          * output */
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(2.0)), // 2-hop path
            ("pool_cb", &token_c, &token_b, MockProtocolSim::new(3.0)), // 2-hop path
        ]);

        // min_hops=2 should skip the 1-hop direct path
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 3, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 100, OrderSide::Sell);

        // Set up derived data with token prices so gas can be deducted
        // This ensures shorter paths are preferred due to lower gas cost
        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algorithm
            .find_best_route(manager.graph(), market, Some(derived), &order)
            .await
            .unwrap();

        // Should use 2-hop path (A->C->B), not the direct 1-hop path
        assert_eq!(result.route().swaps().len(), 2, "Should use 2-hop path due to min_hops=2");
        assert_eq!(result.route().swaps()[0].component_id(), "pool_ac");
        assert_eq!(result.route().swaps()[1].component_id(), "pool_cb");
    }

    #[tokio::test]
    async fn test_find_best_route_respects_max_hops() {
        // Setup: Only path is A->B->C (2 hops)
        // With max_hops=1, should return NoPath error
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        // max_hops=1 cannot reach C from A (needs 2 hops)
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "Should return NoPath when max_hops is insufficient"
        );
    }

    #[tokio::test]
    async fn test_find_best_route_timeout_returns_best_so_far() {
        // Setup: Many parallel paths to process
        // With very short timeout, should return the best route found before timeout
        // or Timeout error if no route was completed
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Create many parallel pools to ensure multiple paths need processing
        let (market, manager) = setup_market(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(1.0)),
            ("pool2", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool3", &token_a, &token_b, MockProtocolSim::new(3.0)),
            ("pool4", &token_a, &token_b, MockProtocolSim::new(4.0)),
            ("pool5", &token_a, &token_b, MockProtocolSim::new(5.0)),
        ]);

        // timeout=0ms should timeout after processing some paths
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(0), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await;

        // With 0ms timeout, we either get:
        // - A route (if at least one path completed before timeout check)
        // - Timeout error (if no path completed)
        // Both are valid outcomes - the key is we don't hang
        match result {
            Ok(r) => {
                // If we got a route, verify it's valid
                assert_eq!(r.route().swaps().len(), 1);
            }
            Err(AlgorithmError::Timeout { .. }) => {
                // Timeout is also acceptable
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    // ==================== Algorithm Trait Getter Tests ====================

    #[rstest::rstest]
    #[case::default_config(1, 3, 50)]
    #[case::single_hop_only(1, 1, 100)]
    #[case::multi_hop_min(2, 5, 200)]
    #[case::zero_timeout(1, 3, 0)]
    #[case::large_values(10, 100, 10000)]
    fn test_algorithm_config_getters(
        #[case] min_hops: usize,
        #[case] max_hops: usize,
        #[case] timeout_ms: u64,
    ) {
        use crate::algorithm::Algorithm;

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(min_hops, max_hops, Duration::from_millis(timeout_ms), None)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(algorithm.max_hops, max_hops);
        assert_eq!(algorithm.timeout, Duration::from_millis(timeout_ms));
        assert_eq!(algorithm.name(), "most_liquid");
    }

    #[test]
    fn test_algorithm_default_config() {
        use crate::algorithm::Algorithm;

        let algorithm = MostLiquidAlgorithm::new();

        assert_eq!(algorithm.max_hops, 3);
        assert_eq!(algorithm.timeout, Duration::from_millis(500));
        assert_eq!(algorithm.name(), "most_liquid");
    }

    // ==================== Configuration Validation Tests ====================

    #[tokio::test]
    async fn test_find_best_route_respects_max_routes_cap() {
        // 4 parallel pools. Score = spot_price * min_depth.
        // In tests, depth comes from get_limits().0 (sell_limit), which is
        // liquidity / (spot_price * (1 - fee)). With fee=0: depth = liquidity / spot_price.
        // We vary liquidity to create a clear score ranking:
        //   pool4 (score = 1.0 * 4M/1.0 = 4M)
        //   pool3 (score = 2.0 * 3M/2.0 = 3M)
        //   pool2 (score = 3.0 * 2M/3.0 = 2M)
        //   pool1 (score = 4.0 * 1M/4.0 = 1M)
        //
        // With max_routes=2, only pool4 and pool3 are simulated.
        // pool1 has the best simulation output (4x) but the lowest score,
        // so it's excluded by the cap.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(4.0).with_liquidity(1_000_000)),
            ("pool2", &token_a, &token_b, MockProtocolSim::new(3.0).with_liquidity(2_000_000)),
            ("pool3", &token_a, &token_b, MockProtocolSim::new(2.0).with_liquidity(3_000_000)),
            ("pool4", &token_a, &token_b, MockProtocolSim::new(1.0).with_liquidity(4_000_000)),
        ]);

        // Cap at 2: only the two highest-scored paths are simulated
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), Some(2)).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 1000, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await
            .unwrap();

        // pool1 has the best simulation output (4x) but lowest score, so it's
        // excluded by the cap. Among the top-2 scored (pool4=4M, pool3=3M),
        // pool3 gives the best simulation output (2x vs 1x).
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "pool3");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(2000u64));
    }

    #[tokio::test]
    async fn test_find_best_route_no_cap_when_max_routes_is_none() {
        // Same setup but no cap — pool1 (best output) should win.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(4.0).with_liquidity(1_000_000)),
            ("pool2", &token_a, &token_b, MockProtocolSim::new(3.0).with_liquidity(2_000_000)),
            ("pool3", &token_a, &token_b, MockProtocolSim::new(2.0).with_liquidity(3_000_000)),
            ("pool4", &token_a, &token_b, MockProtocolSim::new(1.0).with_liquidity(4_000_000)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 1000, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, &order)
            .await
            .unwrap();

        // All 4 paths simulated, pool1 wins with best output (4x)
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "pool1");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(4000u64));
    }

    #[test]
    fn test_algorithm_config_rejects_zero_max_routes() {
        let result = AlgorithmConfig::new(1, 3, Duration::from_millis(100), Some(0));
        assert!(matches!(
            result,
            Err(AlgorithmError::InvalidConfiguration { reason }) if reason.contains("max_routes must be at least 1")
        ));
    }

    #[test]
    fn test_algorithm_config_rejects_zero_min_hops() {
        let result = AlgorithmConfig::new(0, 3, Duration::from_millis(100), None);
        assert!(matches!(
            result,
            Err(AlgorithmError::InvalidConfiguration { reason }) if reason.contains("min_hops must be at least 1")
        ));
    }

    #[test]
    fn test_algorithm_config_rejects_min_greater_than_max() {
        let result = AlgorithmConfig::new(5, 3, Duration::from_millis(100), None);
        assert!(matches!(
            result,
            Err(AlgorithmError::InvalidConfiguration { reason }) if reason.contains("cannot exceed")
        ));
    }
}
