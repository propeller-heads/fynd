//! Split-routing algorithm.
//!
//! Single-path algorithms (MostLiquid, BellmanFord) send the whole order through one route. For
//! large orders, price impact makes it better to split the order across several routes so the
//! marginal price stays low. This algorithm:
//!
//! 1. Enumerates candidate paths (BFS, reusing `MostLiquidAlgorithm::find_paths`).
//! 2. Simulates each at the full amount and keeps the best single-path result (this alone matches
//!    or beats greedy Bellman-Ford because it re-simulates many candidates end-to-end).
//! 3. Picks a set of **pool-disjoint** paths so their allocations never interfere on-chain.
//! 4. Water-fills the order across that set: each chunk goes to the path with the best *net*
//!    marginal output, and a path is only activated when its first chunk's marginal output covers
//!    its gas.
//! 5. Returns whichever is better: the split route or the best single path (so it never loses to a
//!    single-path solver).

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim, tycho_core::models::Address,
};

use super::{
    most_liquid::DepthAndPrice, Algorithm, AlgorithmConfig, MostLiquidAlgorithm, NoPathReason,
};
use crate::{
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, Path, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult, Swap},
    AlgorithmError,
};

/// Maximum candidate paths simulated per order (after heuristic ranking). Set high so the cheap
/// spot×depth pre-ranking never drops the true best single path (which would let a single-path
/// solver beat us); the per-solve timeout bounds the work for hub tokens with many paths.
const DEFAULT_MAX_CANDIDATES: usize = 5000;
/// Maximum number of parallel (pool-disjoint) paths in a split.
const DEFAULT_MAX_PATHS: usize = 4;
/// Number of chunks the order is divided into for water-filling.
const DEFAULT_NUM_CHUNKS: usize = 20;

/// Routes orders by splitting them across pool-disjoint paths to minimize price impact.
pub struct SplitAlgorithm {
    min_hops: usize,
    max_hops: usize,
    timeout: Duration,
    /// Cap on candidate paths simulated (defaults to `max_routes` or [`DEFAULT_MAX_CANDIDATES`]).
    max_candidates: usize,
    /// Max parallel paths in a split.
    max_paths: usize,
    /// Number of water-fill chunks.
    num_chunks: usize,
    connector_tokens: Option<HashSet<Address>>,
}

impl SplitAlgorithm {
    /// Creates a new `SplitAlgorithm` from an [`AlgorithmConfig`].
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self {
            min_hops: config.min_hops(),
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            max_candidates: config
                .max_routes()
                .unwrap_or(DEFAULT_MAX_CANDIDATES)
                .max(DEFAULT_MAX_PATHS),
            max_paths: DEFAULT_MAX_PATHS,
            num_chunks: DEFAULT_NUM_CHUNKS,
            connector_tokens: config.connector_tokens().cloned(),
        })
    }

    /// Simulates a single path at `amount`, returning `(gross_output, total_gas)`.
    ///
    /// Handles intra-path pool reuse via per-pool state overrides, like
    /// [`MostLiquidAlgorithm::simulate_path`], but without allocating `Swap`s — used for the many
    /// marginal probes during water-filling.
    fn simulate_amount(
        path: &Path<DepthAndPrice>,
        market: &MarketState,
        amount: BigUint,
    ) -> Option<(BigUint, BigUint)> {
        let mut current = amount;
        let mut total_gas = BigUint::zero();
        let mut overrides: HashMap<&ComponentId, Box<dyn ProtocolSim>> = HashMap::new();

        for (address_in, edge, address_out) in path.iter() {
            let token_in = market.get_token(address_in)?;
            let token_out = market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let base = market.get_simulation_state(component_id)?;
            let state = overrides
                .get(component_id)
                .map(Box::as_ref)
                .unwrap_or(base);
            let result = state
                .get_amount_out(current.clone(), token_in, token_out)
                .ok()?;
            total_gas += &result.gas;
            overrides.insert(component_id, result.new_state);
            current = result.amount;
        }
        Some((current, total_gas))
    }

    /// Converts a gas amount to output-token terms. Returns `None` if no price is available.
    fn gas_cost_in_token(
        total_gas: &BigUint,
        gas_price_wei: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        token_out: &Address,
    ) -> Option<BigUint> {
        let price = token_prices?.get(token_out)?;
        if price.denominator.is_zero() {
            return None;
        }
        Some(total_gas * gas_price_wei * &price.numerator / &price.denominator)
    }

    /// Greedily selects pool-disjoint paths from `ranked` (best first), up to `max_paths`.
    fn select_disjoint<'a>(
        ranked: &[(usize, &'a Path<'a, DepthAndPrice>)],
        max_paths: usize,
    ) -> Vec<usize> {
        let mut used_components: HashSet<&ComponentId> = HashSet::new();
        let mut selected = Vec::new();
        for (idx, path) in ranked {
            let components: Vec<&ComponentId> = path
                .edge_iter()
                .iter()
                .map(|e| &e.component_id)
                .collect();
            if components
                .iter()
                .any(|c| used_components.contains(*c))
            {
                continue;
            }
            for c in components {
                used_components.insert(c);
            }
            selected.push(*idx);
            if selected.len() >= max_paths {
                break;
            }
        }
        selected
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
        let start = Instant::now();
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

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

        // Step 1: enumerate candidate paths and rank by the MostLiquid heuristic.
        let all_paths = MostLiquidAlgorithm::find_paths(
            graph,
            order.token_in(),
            order.token_out(),
            self.min_hops,
            self.max_hops,
            self.connector_tokens.as_ref(),
        )?;
        if all_paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        // Rank by the cheap spot×depth heuristic, but never *drop* a path for lacking edge weights:
        // pools with missing derived data (partial computation failures) are still routable and can
        // hold the best route, so unscored paths are simply ranked last and still simulated.
        let mut scored: Vec<(Path<DepthAndPrice>, f64)> = all_paths
            .into_iter()
            .map(|p| {
                let s = MostLiquidAlgorithm::try_score_path(&p).unwrap_or(f64::MIN);
                (p, s)
            })
            .collect();
        scored.sort_by(|(_, a), (_, b)| {
            b.partial_cmp(a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(self.max_candidates);

        // Step 2: brief lock — gas price + market subset for simulation.
        let component_ids: HashSet<ComponentId> = scored
            .iter()
            .flat_map(|(p, _)| {
                p.edge_iter()
                    .iter()
                    .map(|e| e.component_id.clone())
            })
            .collect();
        let market = {
            let view = match label.as_ref() {
                Some(l) => market
                    .read_labeled(l)
                    .await
                    .map_err(|e| AlgorithmError::Other(e.to_string()))?,
                None => market.read().await,
            };
            if view.gas_price().is_none() {
                return Err(AlgorithmError::DataNotFound { kind: "gas price", id: None });
            }
            let subset = view.extract_subset_with_overlay(&component_ids);
            drop(view);
            subset
        };
        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();

        // Step 3: simulate each candidate at the full amount; keep the best single-path result and
        // the full-amount gross output for ranking the disjoint selection.
        let paths: Vec<Path<DepthAndPrice>> = scored
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let mut best_single: Option<RouteResult> = None;
        let mut full_outputs: Vec<(usize, BigUint)> = Vec::new();
        let timeout_ms = self.timeout.as_millis() as u64;

        for (idx, path) in paths.iter().enumerate() {
            if start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let Ok(result) = MostLiquidAlgorithm::simulate_path(
                path,
                &market,
                token_prices.as_ref(),
                amount_in.clone(),
            ) else {
                continue;
            };
            let gross = result
                .route()
                .swaps()
                .last()
                .map(|s| s.amount_out().clone())
                .unwrap_or_else(BigUint::zero);
            full_outputs.push((idx, gross));
            if best_single
                .as_ref()
                .map(|b| result.net_amount_out() > b.net_amount_out())
                .unwrap_or(true)
            {
                best_single = Some(result);
            }
        }

        let best_single = best_single.ok_or(AlgorithmError::InsufficientLiquidity)?;

        // Step 4: rank by full-amount output and pick pool-disjoint paths.
        full_outputs.sort_by(|(_, a), (_, b)| b.cmp(a));
        let ranked: Vec<(usize, &Path<DepthAndPrice>)> = full_outputs
            .iter()
            .map(|(idx, _)| (*idx, &paths[*idx]))
            .collect();
        let disjoint = Self::select_disjoint(&ranked, self.max_paths);

        // A single path can't be split — return the best single-path result.
        if disjoint.len() < 2 {
            return Ok(best_single);
        }

        // Step 5: water-fill the order across the disjoint paths by net marginal output.
        let num_chunks = self.num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            // Amount too small to chunk meaningfully — single path is fine.
            return Ok(best_single);
        }
        let remainder = &amount_in - &base_chunk * num_chunks;

        let mut alloc: Vec<BigUint> = vec![BigUint::zero(); disjoint.len()];
        let mut cur_out: Vec<BigUint> = vec![BigUint::zero(); disjoint.len()];
        let mut used: Vec<bool> = vec![false; disjoint.len()];

        for chunk_idx in 0..num_chunks {
            if start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };

            let mut best: Option<(usize, BigInt, BigUint)> = None; // (path_i, net_marginal, new_out)
            for (i, &path_idx) in disjoint.iter().enumerate() {
                let probe_in = &alloc[i] + &chunk;
                let Some((probe_out, probe_gas)) =
                    Self::simulate_amount(&paths[path_idx], &market, probe_in)
                else {
                    continue;
                };
                let gross_marginal =
                    BigInt::from(probe_out.clone()) - BigInt::from(cur_out[i].clone());
                let net_marginal = if used[i] {
                    gross_marginal
                } else {
                    let activation = Self::gas_cost_in_token(
                        &probe_gas,
                        &gas_price,
                        token_prices.as_ref(),
                        order.token_out(),
                    )
                    .map(BigInt::from)
                    .unwrap_or_else(BigInt::zero);
                    gross_marginal - activation
                };
                if best
                    .as_ref()
                    .map(|(_, m, _)| &net_marginal > m)
                    .unwrap_or(true)
                {
                    best = Some((i, net_marginal, probe_out));
                }
            }

            let Some((best_i, _, new_out)) = best else {
                break;
            };
            alloc[best_i] += &chunk;
            cur_out[best_i] = new_out;
            used[best_i] = true;
        }

        // Step 6: build the combined split route and compute its net output.
        let mut swaps: Vec<Swap> = Vec::new();
        let mut total_gross = BigUint::zero();
        let mut total_gas = BigUint::zero();
        for (i, &path_idx) in disjoint.iter().enumerate() {
            if alloc[i].is_zero() {
                continue;
            }
            let Ok(leg) = MostLiquidAlgorithm::simulate_path(
                &paths[path_idx],
                &market,
                token_prices.as_ref(),
                alloc[i].clone(),
            ) else {
                continue;
            };
            let leg_route = leg.into_route();
            let leg_gross = leg_route
                .swaps()
                .last()
                .map(|s| s.amount_out().clone())
                .unwrap_or_else(BigUint::zero);
            total_gross += &leg_gross;
            total_gas += leg_route.total_gas();
            let fraction = ratio(&alloc[i], &amount_in);
            for swap in leg_route.into_swaps() {
                swaps.push(swap.with_split(fraction));
            }
        }

        if swaps.is_empty() {
            return Ok(best_single);
        }

        let gas_cost = Self::gas_cost_in_token(
            &total_gas,
            &gas_price,
            token_prices.as_ref(),
            order.token_out(),
        );
        let split_net = match gas_cost {
            Some(cost) => BigInt::from(total_gross) - BigInt::from(cost),
            None => BigInt::from(total_gross),
        };

        // Step 7: return whichever is better — never lose to the single-path solution.
        if &split_net > best_single.net_amount_out() {
            let route = Route::new(swaps, HashMap::new());
            Ok(RouteResult::new(route, split_net, gas_price))
        } else {
            Ok(best_single)
        }
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("Conflicting Computation Requirements")
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Computes `numerator / denominator` as an `f64` fraction in `[0, 1]`.
fn ratio(numerator: &BigUint, denominator: &BigUint) -> f64 {
    use num_traits::ToPrimitive;
    let n = numerator.to_f64().unwrap_or(0.0);
    let d = denominator.to_f64().unwrap_or(1.0);
    if d == 0.0 {
        0.0
    } else {
        n / d
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use num_traits::ToPrimitive;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{addr, component, token_with_decimals},
        feed::market_data::MarketState,
        offline::{prepare, OfflineSolver},
        types::{quote::OrderSide, BlockInfo},
        MostLiquidAlgorithm,
    };

    fn weth_usdc_pool(weth_reserve: u128, usdc_reserve: u128) -> UniswapV2State {
        UniswapV2State::new(
            U256::from(weth_reserve) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(usdc_reserve) * U256::from(10u64).pow(U256::from(6u64)),
        )
    }

    /// Two equally-deep WETH/USDC pools: a large order should split ~50/50 and beat any single
    /// path.
    #[tokio::test]
    async fn split_beats_single_path_on_two_equal_pools() {
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

        let (md, derived) = prepare(market, weth.address.clone(), 2, 0.01)
            .await
            .expect("prepare");

        // Large order: 500 WETH — heavy price impact, so splitting clearly wins.
        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(500u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();

        let split_solver = OfflineSolver::new(
            md.clone(),
            derived.clone(),
            SplitAlgorithm::with_config(config.clone()).unwrap(),
        )
        .await;
        let split = split_solver
            .solve(&order)
            .await
            .expect("split solves");

        let ml_solver =
            OfflineSolver::new(md, derived, MostLiquidAlgorithm::with_config(config).unwrap())
                .await;
        let ml = ml_solver
            .solve(&order)
            .await
            .expect("ml solves");

        assert_eq!(split.num_paths, 2, "large order should use both pools");
        assert!(
            split.gross_amount_out > ml.gross_amount_out,
            "split ({}) should beat single-path ({})",
            split.gross_amount_out,
            ml.gross_amount_out
        );

        // Splitting 50/50 across two identical pools should be ~20% better than one pool here.
        let gain = split.gross_amount_out.to_f64().unwrap() / ml.gross_amount_out.to_f64().unwrap();
        assert!(gain > 1.15, "expected >15% gain from splitting, got {:.3}x", gain);
    }

    /// A tiny order shouldn't split (gas/impact make one pool optimal) and must not lose to
    /// single-path.
    #[tokio::test]
    async fn small_order_does_not_lose_to_single_path() {
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

        let (md, derived) = prepare(market, weth.address.clone(), 2, 0.01)
            .await
            .expect("prepare");

        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(10u64).pow(15), // 0.001 WETH
            OrderSide::Sell,
            addr(0xFF),
        );
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();

        let split_solver = OfflineSolver::new(
            md.clone(),
            derived.clone(),
            SplitAlgorithm::with_config(config.clone()).unwrap(),
        )
        .await;
        let split = split_solver
            .solve(&order)
            .await
            .expect("split solves");
        let ml_solver =
            OfflineSolver::new(md, derived, MostLiquidAlgorithm::with_config(config).unwrap())
                .await;
        let ml = ml_solver
            .solve(&order)
            .await
            .expect("ml solves");

        assert!(
            split.net_amount_out >= ml.net_amount_out,
            "split must never lose to single-path: split={} ml={}",
            split.net_amount_out,
            ml.net_amount_out
        );
    }
}
