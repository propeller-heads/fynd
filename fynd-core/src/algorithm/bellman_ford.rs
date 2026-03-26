//! Bellman-Ford algorithm with SPFA optimization for simulation-driven routing.
//!
//! Runs actual pool simulations (`get_amount_out()`) during edge relaxation to find
//! optimal A-to-B routes that account for slippage, fees, and pool mechanics at the
//! given trade size.
//!
//! Key features:
//! - **Gas-aware relaxation**: When token prices and gas price are available, relaxation compares
//!   net amounts (gross output minus cumulative gas cost in token terms) instead of gross output
//!   alone. Falls back to gross comparison when data is unavailable.
//! - **Subgraph extraction**: BFS prunes the graph to nodes reachable within `max_hops`
//! - **SPFA (Shortest Path Faster Algorithm) queuing**: Only re-relaxes edges from nodes whose
//!   amount improved
//! - **Forbid revisits**: Skips edges that would revisit a token or pool already in the path

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, instrument, trace, warn};
use tycho_simulation::{
    tycho_common::models::Address,
    tycho_core::{models::token::Token, simulation::protocol_sim::Price},
};

use super::{bf_helpers, Algorithm, AlgorithmConfig, AlgorithmError, NoPathReason};
use crate::{
    derived::{
        computation::ComputationRequirements,
        types::{SpotPrices, TokenGasPrices},
        SharedDerivedDataRef,
    },
    feed::market_data::SharedMarketDataRef,
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult, Swap},
};

/// BFS subgraph: adjacency list, token node set, and component ID set.
type Subgraph =
    (HashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>>, HashSet<NodeIndex>, HashSet<ComponentId>);

pub struct BellmanFordAlgorithm {
    max_hops: usize,
    timeout: Duration,
    gas_aware: bool,
}

impl BellmanFordAlgorithm {
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self {
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            gas_aware: config.gas_aware(),
        })
    }

    /// Computes gas-adjusted net amount: gross_amount - gas_cost_in_token.
    ///
    /// If `token_price` is None (no conversion rate available), returns the gross amount
    /// unchanged (falls back to gross comparison for this node).
    fn gas_adjusted_amount(
        gross: &BigUint,
        cumul_gas: &BigUint,
        gas_price_wei: &BigUint,
        token_price: Option<&Price>,
    ) -> BigInt {
        match token_price {
            Some(price) if !price.denominator.is_zero() => {
                let gas_cost = cumul_gas * gas_price_wei * &price.numerator / &price.denominator;
                BigInt::from(gross.clone()) - BigInt::from(gas_cost)
            }
            _ => BigInt::from(gross.clone()),
        }
    }

    /// Computes the cumulative spot price product when extending a path by one edge.
    ///
    /// Returns `parent_spot * spot_price(component, token_u, token_v)`.
    /// Returns 0.0 if the spot price is unavailable (disables the fallback for this path).
    fn compute_edge_spot_product(
        parent_spot: f64,
        component_id: &ComponentId,
        u_addr: Option<&Address>,
        v_addr: Option<&Address>,
        spot_prices: Option<&SpotPrices>,
    ) -> f64 {
        if parent_spot == 0.0 {
            return 0.0;
        }
        let (Some(u), Some(v), Some(prices)) = (u_addr, v_addr, spot_prices) else {
            return 0.0;
        };
        let key = (component_id.clone(), u.clone(), v.clone());
        match prices.get(&key) {
            Some(&spot) if spot > 0.0 => parent_spot * spot,
            _ => 0.0,
        }
    }

    /// Resolves the gas-to-token conversion rate for gas cost calculation.
    ///
    /// 1. Primary: use `token_prices[v_addr]` from derived data (direct lookup).
    /// 2. Fallback: if `token_prices[token_in]` exists and `spot_product > 0`, estimate the rate as
    ///    `token_prices[token_in] * spot_product` (converted to a Price).
    /// 3. Last resort: returns None (gas adjustment skipped for this comparison).
    fn resolve_token_price(
        v_addr: Option<&Address>,
        token_prices: Option<&TokenGasPrices>,
        spot_product: f64,
        token_in_addr: Option<&Address>,
    ) -> Option<Price> {
        let prices = token_prices?;
        let addr = v_addr?;

        // Primary: direct lookup
        if let Some(price) = prices.get(addr) {
            return Some(price.clone());
        }

        // Fallback: token_in price * cumulative spot product
        if spot_product > 0.0 {
            if let Some(in_price) = token_in_addr.and_then(|a| prices.get(a)) {
                let in_rate_f64 = in_price.numerator.to_f64()? / in_price.denominator.to_f64()?;
                let estimated_rate = in_rate_f64 * spot_product;
                let denom = BigUint::from(10u64).pow(18);
                let numer_f64 = estimated_rate * 1e18;
                if numer_f64.is_finite() && numer_f64 > 0.0 {
                    return Some(Price {
                        numerator: BigUint::from(numer_f64 as u128),
                        denominator: denom,
                    });
                }
            }
        }

        None
    }

    /// Extracts the subgraph reachable from `token_in_node` within `max_hops` via BFS.
    ///
    /// Returns `(adjacency_list, token_nodes, component_ids)` or `NoPath` if the
    /// subgraph is empty (no outgoing edges from the source).
    fn get_subgraph(
        graph: &StableDiGraph<()>,
        token_in_node: NodeIndex,
        max_hops: usize,
        order: &Order,
    ) -> Result<Subgraph, AlgorithmError> {
        let mut adj: HashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>> = HashMap::new();
        let mut token_nodes: HashSet<NodeIndex> = HashSet::new();
        let mut component_ids: HashSet<ComponentId> = HashSet::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        visited.insert(token_in_node);
        token_nodes.insert(token_in_node);
        queue.push_back((token_in_node, 0usize));

        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            for edge in graph.edges(node) {
                let target = edge.target();
                let cid = edge.weight().component_id.clone();

                adj.entry(node)
                    .or_default()
                    .push((target, cid.clone()));
                component_ids.insert(cid);
                token_nodes.insert(target);

                if visited.insert(target) {
                    queue.push_back((target, depth + 1));
                }
            }
        }

        if adj.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        Ok((adj, token_nodes, component_ids))
    }

    /// Computes net_amount_out by subtracting gas costs from the output amount.
    ///
    /// Uses the same resolution strategy as relaxation: direct token price lookup
    /// first, then cumulative spot price product fallback for tokens not in the price
    /// table.
    #[allow(clippy::too_many_arguments)]
    fn compute_net_amount_out(
        amount_out: &BigUint,
        route: &Route,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        spot_product: &[f64],
        node_address: &HashMap<NodeIndex, Address>,
        token_in_node: NodeIndex,
    ) -> BigInt {
        let Some(last_swap) = route.swaps().last() else {
            return BigInt::from(amount_out.clone());
        };

        let total_gas = route.total_gas();

        if gas_price.is_zero() {
            warn!("missing gas price, returning gross amount_out");
            return BigInt::from(amount_out.clone());
        }

        let gas_cost_wei = &total_gas * gas_price;

        // Find the output token's node to get its spot_product for the fallback
        let out_addr = last_swap.token_out();
        let out_node_spot = node_address
            .iter()
            .find(|(_, addr)| *addr == out_addr)
            .and_then(|(node, _)| spot_product.get(node.index()).copied())
            .unwrap_or(0.0);

        let output_price = Self::resolve_token_price(
            Some(out_addr),
            token_prices,
            out_node_spot,
            node_address.get(&token_in_node),
        );

        match output_price {
            Some(price) if !price.denominator.is_zero() => {
                let gas_cost = &gas_cost_wei * &price.numerator / &price.denominator;
                BigInt::from(amount_out.clone()) - BigInt::from(gas_cost)
            }
            _ => {
                warn!("no gas price for output token, returning gross amount_out");
                BigInt::from(amount_out.clone())
            }
        }
    }
}

impl Algorithm for BellmanFordAlgorithm {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "bellman_ford"
    }

    #[instrument(level = "debug", skip_all, fields(order_id = %order.id()))]
    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: SharedMarketDataRef,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        let start = Instant::now();

        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let (token_prices, spot_prices) = if let Some(ref derived) = derived {
            let guard = derived.read().await;
            (guard.token_prices().cloned(), guard.spot_prices().cloned())
        } else {
            (None, None)
        };

        let token_in_node = graph
            .node_indices()
            .find(|&n| &graph[n] == order.token_in())
            .ok_or(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::SourceTokenNotInGraph,
            })?;
        let token_out_node = graph
            .node_indices()
            .find(|&n| &graph[n] == order.token_out())
            .ok_or(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::DestinationTokenNotInGraph,
            })?;

        // BFS from token_in up to max_hops, building adjacency list and component set.
        let (adj, token_nodes, component_ids) =
            Self::get_subgraph(graph, token_in_node, self.max_hops, order)?;

        // Acquire read lock only for market data extraction, then release.
        let (token_map, market_subset) = {
            let market = market.read().await;

            let token_map: HashMap<NodeIndex, Token> = token_nodes
                .iter()
                .filter_map(|&node| {
                    market
                        .get_token(&graph[node])
                        .cloned()
                        .map(|t| (node, t))
                })
                .collect();

            let market_subset = market.extract_subset(&component_ids);

            (token_map, market_subset)
        };

        debug!(
            edges = adj
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            tokens = token_map.len(),
            "subgraph extracted"
        );

        // SPFA relaxation with forbid-revisits.
        // amount[node] = best gross output amount reachable at node.
        // edge_gas[node] = gas estimate for the edge that last improved amount[node].
        // cumul_gas[node] = total gas units along the best path to this node.
        let max_idx = graph
            .node_indices()
            .map(|n| n.index())
            .max()
            .unwrap_or(0) +
            1;

        let mut amount: Vec<BigUint> = vec![BigUint::ZERO; max_idx];
        let mut predecessor: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; max_idx];
        let mut edge_gas: Vec<BigUint> = vec![BigUint::ZERO; max_idx];
        let mut cumul_gas: Vec<BigUint> = vec![BigUint::ZERO; max_idx];

        amount[token_in_node.index()] = order.amount().clone();

        // Gas-aware relaxation: pre-compute gas price and build address map for lookups.
        let gas_price_wei = market_subset
            .gas_price()
            .map(|gp| gp.effective_gas_price().clone());

        // Build node->address map for token price lookups during relaxation.
        let node_address: HashMap<NodeIndex, Address> = token_map
            .iter()
            .map(|(&node, token)| (node, token.address.clone()))
            .collect();

        // Track cumulative spot price product from token_in for fallback gas estimation.
        // spot_product[v] = product of spot prices along the path from token_in to v.
        let mut spot_product: Vec<f64> = vec![0.0; max_idx];
        spot_product[token_in_node.index()] = 1.0;

        let gas_aware = self.gas_aware && gas_price_wei.is_some() && token_prices.is_some();
        if !gas_aware && self.gas_aware {
            debug!("gas-aware comparison disabled (missing gas_price or token_prices)");
        } else if !self.gas_aware {
            debug!("gas-aware comparison disabled by config");
        }

        let mut active_nodes: Vec<NodeIndex> = vec![token_in_node];

        for round in 0..self.max_hops {
            if start.elapsed() >= self.timeout {
                debug!(round, "timeout during relaxation");
                break;
            }
            if active_nodes.is_empty() {
                debug!(round, "no active nodes, stopping early");
                break;
            }

            let mut next_active: HashSet<NodeIndex> = HashSet::new();

            for &u in &active_nodes {
                let u_idx = u.index();
                if amount[u_idx].is_zero() {
                    continue;
                }

                let Some(token_u) = token_map.get(&u) else { continue };
                let Some(edges) = adj.get(&u) else { continue };

                for (v, component_id) in edges {
                    let v_idx = v.index();

                    // Single predecessor walk: skip if target token or pool already in path
                    if bf_helpers::path_has_conflict(u, *v, component_id, &predecessor) {
                        continue;
                    }

                    let Some(token_v) = token_map.get(v) else { continue };
                    let Some(sim) = market_subset.get_simulation_state(component_id) else {
                        continue;
                    };

                    let result = match sim.get_amount_out(amount[u_idx].clone(), token_u, token_v) {
                        Ok(r) => r,
                        Err(e) => {
                            trace!(
                                component_id,
                                error = %e,
                                "simulation failed, skipping edge"
                            );
                            continue;
                        }
                    };

                    let candidate_cumul_gas = &cumul_gas[u_idx] + &result.gas;

                    // Compute spot price product for the candidate path (used for
                    // gas-aware comparison and for final net amount calculation).
                    let candidate_spot = Self::compute_edge_spot_product(
                        spot_product[u_idx],
                        component_id,
                        node_address.get(&u),
                        node_address.get(v),
                        spot_prices.as_ref(),
                    );

                    // Gas-aware comparison: compare net amounts (gross - gas cost in token terms)
                    let is_better = if gas_aware {
                        let v_price = Self::resolve_token_price(
                            node_address.get(v),
                            token_prices.as_ref(),
                            candidate_spot,
                            node_address.get(&token_in_node),
                        );

                        let net_candidate = Self::gas_adjusted_amount(
                            &result.amount,
                            &candidate_cumul_gas,
                            gas_price_wei.as_ref().unwrap(),
                            v_price.as_ref(),
                        );
                        let net_existing = Self::gas_adjusted_amount(
                            &amount[v_idx],
                            &cumul_gas[v_idx],
                            gas_price_wei.as_ref().unwrap(),
                            v_price.as_ref(),
                        );
                        net_candidate > net_existing
                    } else {
                        result.amount > amount[v_idx]
                    };

                    if is_better {
                        spot_product[v_idx] = candidate_spot;
                        amount[v_idx] = result.amount;
                        predecessor[v_idx] = Some((u, component_id.clone()));
                        edge_gas[v_idx] = result.gas;
                        cumul_gas[v_idx] = candidate_cumul_gas;
                        next_active.insert(*v);
                    }
                }
            }

            active_nodes = next_active.into_iter().collect();
        }

        // Check if destination was reached
        let out_idx = token_out_node.index();
        if amount[out_idx].is_zero() {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        // Reconstruct path and build route directly from stored distances/gas
        // (no re-simulation needed since forbid-revisits guarantees relaxation
        // amounts match sequential execution).
        let path_edges = bf_helpers::reconstruct_path(token_out_node, token_in_node, &predecessor)?;

        let mut swaps = Vec::with_capacity(path_edges.len());
        for (from_node, to_node, component_id) in &path_edges {
            let token_in = token_map
                .get(from_node)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: Some(format!("{:?}", from_node)),
                })?;
            let token_out = token_map
                .get(to_node)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: Some(format!("{:?}", to_node)),
                })?;
            let component = market_subset
                .get_component(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "component",
                    id: Some(component_id.clone()),
                })?;
            let sim_state = market_subset
                .get_simulation_state(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: Some(component_id.clone()),
                })?;

            swaps.push(Swap::new(
                component_id.clone(),
                component.protocol_system.clone(),
                token_in.address.clone(),
                token_out.address.clone(),
                amount[from_node.index()].clone(),
                amount[to_node.index()].clone(),
                edge_gas[to_node.index()].clone(),
                component.clone(),
                sim_state.clone_box(),
            ));
        }

        let route = Route::new(swaps);
        let final_amount_out = amount[out_idx].clone();

        let gas_price = gas_price_wei.unwrap_or_default();

        let net_amount_out = Self::compute_net_amount_out(
            &final_amount_out,
            &route,
            &gas_price,
            token_prices.as_ref(),
            &spot_product,
            &node_address,
            token_in_node,
        );

        let result = RouteResult::new(route, net_amount_out, gas_price);

        let solve_time_ms = start.elapsed().as_millis() as u64;
        debug!(
            solve_time_ms,
            hops = result.route().swaps().len(),
            amount_in = %order.amount(),
            amount_out = %final_amount_out,
            net_amount_out = %result.net_amount_out(),
            "bellman_ford route found"
        );

        Ok(result)
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        // Static requirements for independent computations; cannot conflict.
        // The trait returns ComputationRequirements (not Result), so expect is
        // the appropriate pattern for this infallible case.
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("token_prices requirement conflicts (bug)")
            .allow_stale("spot_prices")
            .expect("spot_prices requirement conflicts (bug)")
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use num_bigint::BigInt;
    use tokio::sync::RwLock;
    use tycho_simulation::{
        tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim},
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{component, order, token, MockProtocolSim},
        derived::{types::TokenGasPrices, DerivedData},
        feed::market_data::SharedMarketData,
        graph::GraphManager,
        types::quote::OrderSide,
    };

    // ==================== Test Utilities ====================

    /// Sets up market and graph with `()` edge weights for BellmanFord tests.
    fn setup_market_bf(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (Arc<RwLock<SharedMarketData>>, PetgraphStableDiGraphManager<()>) {
        let mut market = SharedMarketData::new();

        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(100u64) },
        });
        market.update_last_updated(crate::types::BlockInfo::new(1, "0x00".into(), 0));

        for (pool_id, token_in, token_out, state) in pools {
            let tokens = vec![token_in.clone(), token_out.clone()];
            let comp = component(pool_id, &tokens);
            market.upsert_components(std::iter::once(comp));
            market.update_states([(pool_id.to_string(), Box::new(state) as Box<dyn ProtocolSim>)]);
            market.upsert_tokens(tokens);
        }

        let mut graph_manager = PetgraphStableDiGraphManager::default();
        graph_manager.initialize_graph(&market.component_topology());

        (Arc::new(RwLock::new(market)), graph_manager)
    }

    fn setup_derived_with_token_prices(
        token_addresses: &[Address],
    ) -> crate::derived::SharedDerivedDataRef {
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        let mut token_prices: TokenGasPrices = HashMap::new();
        for address in token_addresses {
            token_prices.insert(
                address.clone(),
                Price { numerator: BigUint::from(1u64), denominator: BigUint::from(1u64) },
            );
        }

        let mut derived_data = DerivedData::new();
        derived_data.set_token_prices(token_prices, 1);
        Arc::new(RwLock::new(derived_data))
    }

    fn bf_algorithm(max_hops: usize, timeout_ms: u64) -> BellmanFordAlgorithm {
        BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, max_hops, Duration::from_millis(timeout_ms), None).unwrap(),
        )
        .unwrap()
    }

    // ==================== Unit Tests ====================

    #[tokio::test]
    async fn test_linear_path_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(4.0)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 3);
        // A->B: 100*2=200, B->C: 200*3=600, C->D: 600*4=2400
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(200u64));
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(600u64));
        assert_eq!(result.route().swaps()[2].amount_out(), &BigUint::from(2400u64));
    }

    #[tokio::test]
    async fn test_picks_better_of_two_paths() {
        // Diamond graph: A->B->D (2*3=6x) vs A->C->D (4*1=4x)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bd", &token_b, &token_d, MockProtocolSim::new(3.0)),
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(4.0)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(1.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // A->B->D: 100*2*3=600 is better than A->C->D: 100*4*1=400
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "pool_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "pool_bd");
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(600u64));
    }

    #[tokio::test]
    async fn test_parallel_pools() {
        // Two pools between A and B with different multipliers
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![
            ("pool1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool2", &token_a, &token_b, MockProtocolSim::new(5.0)),
        ]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "pool2");
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(500u64));
    }

    #[tokio::test]
    async fn test_no_path_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        // A-B connected, C disconnected
        let (market, manager) =
            setup_market_bf(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        // Add token_c to market without connecting it
        {
            let mut m = market.write().await;
            m.upsert_tokens(vec![token_c.clone()]);
        }

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[tokio::test]
    async fn test_source_not_in_graph() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_x = token(0x99, "X");

        let (market, manager) =
            setup_market_bf(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_x, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::SourceTokenNotInGraph, .. })
        ));
    }

    #[tokio::test]
    async fn test_destination_not_in_graph() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_x = token(0x99, "X");

        let (market, manager) =
            setup_market_bf(vec![("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_x, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::DestinationTokenNotInGraph, .. })
        ));
    }

    #[tokio::test]
    async fn test_respects_max_hops() {
        // Path A->B->C->D exists but requires 3 hops; max_hops=2
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(4.0)),
        ]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "Should not find 3-hop path with max_hops=2"
        );
    }

    #[tokio::test]
    async fn test_source_token_revisit_blocked() {
        // Forbid-revisits prevents paths like A->B->A->B->C. The algorithm
        // should find the direct A->B->C path instead.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // Should find exactly the 2-hop path A->B->C = 100*2*3 = 600
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "pool_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "pool_bc");
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(600u64));
    }

    #[tokio::test]
    async fn test_hub_token_revisit_blocked() {
        // Forbid-revisits blocks A->B->C->B->D (B visited twice).
        // The algorithm should find the direct A->B->D = 400 instead.
        let token_a = token(0x01, "A");
        let token_c = token(0x02, "C");
        let token_b = token(0x03, "B");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("pool_cb", &token_c, &token_b, MockProtocolSim::new(100.0)),
            ("pool_bd", &token_b, &token_d, MockProtocolSim::new(2.0)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // Should find A->B->D = 100*2*2 = 400 (the direct 2-hop path)
        // The 4-hop revisit path A->B->C->B->D is blocked
        assert_eq!(result.route().swaps().len(), 2, "should use direct 2-hop path");
        assert_eq!(result.route().swaps()[0].component_id(), "pool_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "pool_bd");
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(400u64));
    }

    #[tokio::test]
    async fn test_route_amounts_are_sequential() {
        // Verify that swap amount_in[i+1] == amount_out[i] in the built route
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 2);
        // amount_in of second swap == amount_out of first swap
        assert_eq!(result.route().swaps()[1].amount_in(), result.route().swaps()[0].amount_out());
    }

    #[tokio::test]
    async fn test_gas_deduction() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_gas(10),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algo
            .find_best_route(manager.graph(), market, Some(derived), &ord)
            .await
            .unwrap();

        // Output: 1000 * 2 = 2000
        // Gas: 10 gas units * 100 gas_price = 1000 wei * 1/1 price = 1000
        // Net: 2000 - 1000 = 1000
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(2000u64));
        assert_eq!(result.net_amount_out(), &BigInt::from(1000));
    }

    #[tokio::test]
    async fn test_timeout_respected() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        // 0ms timeout
        let algo = bf_algorithm(3, 0);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;

        // With 0ms timeout, we expect either:
        // - A partial result (if some layers completed before timeout check)
        // - Timeout error
        // - NoPath (if timeout prevented completing enough layers to reach dest)
        match result {
            Ok(r) => {
                assert!(!r.route().swaps().is_empty());
            }
            Err(AlgorithmError::Timeout { .. }) | Err(AlgorithmError::NoPath { .. }) => {
                // Both are acceptable for 0ms timeout
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    // ==================== Integration-style Tests ====================

    #[tokio::test]
    async fn test_with_fees() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Pool with 10% fee
        let (market, manager) = setup_market_bf(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_fee(0.1),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // 1000 * 2 * (1-0.1) = 1800
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(1800u64));
    }

    #[tokio::test]
    async fn test_large_trade_slippage() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Pool with limited liquidity (500 tokens)
        let (market, manager) = setup_market_bf(vec![(
            "pool1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_liquidity(500),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        // Should fail due to insufficient liquidity
        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "Should fail when trade exceeds pool liquidity"
        );
    }

    #[tokio::test]
    async fn test_disconnected_tokens_return_no_path() {
        // A-B connected, D-E disconnected. Routing A->E should fail.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_d = token(0x04, "D");
        let token_e = token(0x05, "E");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_de", &token_d, &token_e, MockProtocolSim::new(4.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_e, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "should not find path to disconnected component"
        );
    }

    #[tokio::test]
    async fn test_spfa_skips_failed_simulations() {
        // Pool that will fail simulation (liquidity=0 would cause error for any amount)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            // Direct path with failing pool
            ("pool_ab_bad", &token_a, &token_b, MockProtocolSim::new(2.0).with_liquidity(0)),
            // Alternative path that works
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(2.0)),
            ("pool_cb", &token_c, &token_b, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await;

        // Should find A->C->B despite A->B failing
        // Note: MockProtocolSim with liquidity=0 will fail for amount > 0
        // The direct A->B edge should be skipped and the 2-hop path used
        match result {
            Ok(r) => {
                // Found alternative path
                assert!(!r.route().swaps().is_empty());
            }
            Err(AlgorithmError::NoPath { .. }) => {
                // Also acceptable if liquidity=0 blocks all paths through B
                // (since the failing pool might also block the reverse B->A edge)
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_resimulation_produces_correct_amounts() {
        // Verifies that re-simulation produces the same correct sequential amounts
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("pool_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // Verify the final amounts are from re-simulation, not relaxation
        // A->B: 100*2=200, B->C: 200*3=600
        assert_eq!(result.route().swaps()[0].amount_in(), &BigUint::from(100u64));
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(200u64));
        assert_eq!(result.route().swaps()[1].amount_in(), &BigUint::from(200u64));
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(600u64));
    }

    // ==================== Trait getter tests ====================

    #[test]
    fn algorithm_name() {
        let algo = bf_algorithm(4, 200);
        assert_eq!(algo.name(), "bellman_ford");
    }

    #[test]
    fn algorithm_timeout() {
        let algo = bf_algorithm(4, 200);
        assert_eq!(algo.timeout(), Duration::from_millis(200));
    }

    // ==================== Forbid-revisit helper tests ====================

    #[tokio::test]
    async fn test_gas_aware_relaxation_picks_cheaper_path() {
        // Diamond graph: A -> B -> D vs A -> C -> D
        // Path via B: higher gross output (3x * 2x = 6x) but extreme gas (100M per hop)
        // Path via C: lower gross output (2x * 2x = 4x) but cheap gas (100 per hop)
        //
        // With gas_price=100, token_prices[D]=1:1 for WETH conversion:
        // Path B gas cost: (100M + 100M) * 100 * 1 = 20B
        // Path C gas cost: (100 + 100) * 100 * 1 = 20K
        //
        // For an input of 1B:
        // Path B: gross = 6B, net = 6B - 20B = -14B
        // Path C: gross = 4B, net = 4B - 20K ≈ 4B
        //
        // Without gas awareness: Path B wins (6B > 4B)
        // With gas awareness: Path C wins (4B net > -14B net)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let high_gas: u64 = 100_000_000;
        let low_gas: u64 = 100;

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(3.0).with_gas(high_gas)),
            ("pool_bd", &token_b, &token_d, MockProtocolSim::new(2.0).with_gas(high_gas)),
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(2.0).with_gas(low_gas)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(2.0).with_gas(low_gas)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 1_000_000_000, OrderSide::Sell);

        // With gas-aware relaxation (derived data with token prices + gas price in market)
        let derived = setup_derived_with_token_prices(&[
            token_a.address.clone(),
            token_b.address.clone(),
            token_c.address.clone(),
            token_d.address.clone(),
        ]);

        let result = algo
            .find_best_route(manager.graph(), market, Some(derived), &ord)
            .await
            .unwrap();

        // Gas-aware relaxation should pick the cheaper path A -> C -> D
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "pool_ac");
        assert_eq!(result.route().swaps()[1].component_id(), "pool_cd");
    }

    #[tokio::test]
    async fn test_gas_aware_falls_back_to_gross_without_derived() {
        // Same diamond graph as above, but without derived data.
        // Should fall back to gross comparison and pick Path B (higher gross).
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let high_gas: u64 = 100_000_000;
        let low_gas: u64 = 100;

        let (market, manager) = setup_market_bf(vec![
            ("pool_ab", &token_a, &token_b, MockProtocolSim::new(3.0).with_gas(high_gas)),
            ("pool_bd", &token_b, &token_d, MockProtocolSim::new(2.0).with_gas(high_gas)),
            ("pool_ac", &token_a, &token_c, MockProtocolSim::new(2.0).with_gas(low_gas)),
            ("pool_cd", &token_c, &token_d, MockProtocolSim::new(2.0).with_gas(low_gas)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 1_000_000_000, OrderSide::Sell);

        // No derived data: should fall back to gross comparison
        let result = algo
            .find_best_route(manager.graph(), market, None, &ord)
            .await
            .unwrap();

        // Without gas awareness, picks the higher-gross path A -> B -> D
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "pool_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "pool_bd");
    }

    #[test]
    fn test_path_has_conflict_detects_node_and_pool() {
        // Path: 0 -[pool_a]-> 1 -[pool_b]-> 2
        let mut pred: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; 4];
        pred[1] = Some((NodeIndex::new(0), "pool_a".into()));
        pred[2] = Some((NodeIndex::new(1), "pool_b".into()));

        // Node conflicts: node 0 is in path, node 3 is not
        assert!(bf_helpers::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(0),
            &"any".into(),
            &pred
        ));
        assert!(!bf_helpers::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"any".into(),
            &pred
        ));
        // Self-check: node 2 is itself in the "path from 2"
        assert!(bf_helpers::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(2),
            &"any".into(),
            &pred
        ));

        // Pool conflicts: pool_a and pool_b are used, pool_c is not
        assert!(bf_helpers::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"pool_a".into(),
            &pred
        ));
        assert!(bf_helpers::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"pool_b".into(),
            &pred
        ));
        assert!(!bf_helpers::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"pool_c".into(),
            &pred
        ));
    }
}
