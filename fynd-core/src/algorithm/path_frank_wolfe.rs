//! Path-based Frank-Wolfe split-routing algorithm.
//!
//! Wraps [`BellmanFordAlgorithm`] to find multiple candidate paths and then
//! optimally split the input amount across them. The inner BF instance handles
//! single-path discovery; this module layers on the Frank-Wolfe optimisation
//! loop to determine the best split fractions.

use std::time::{Duration, Instant};

use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use tracing::debug;

use super::{
    bellman_ford::{BellmanFordContext, FindRouteOptions},
    split_primitives::{
        build_post_swap_overrides, build_split_route, compute_marginal_price_product,
        evaluate_total_output, golden_section_search, normalize_fractions, simulate_path,
        split_amount, HopDescriptor, MarketOverrides, PathAllocation, SimulatedHop,
    },
    Algorithm, AlgorithmConfig, AlgorithmError, BellmanFordAlgorithm,
};
use crate::{
    derived::{computation::ComputationRequirements, SharedDerivedDataRef},
    feed::market_data::{MarketData, StateLabel},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{quote::Order, OrderSide, Route, RouteResult},
};

/// Tuning parameters for the path-based Frank-Wolfe split-routing loop.
#[derive(Debug, Clone)]
pub struct PathFrankWolfeConfig {
    /// Maximum number of distinct paths to split across.
    pub max_paths: usize,
    /// Cap beyond which no split is attempted (e.g. 0.25 = 25%).
    pub max_probe: f64,
    /// Minimum share of total input allocated to any single path (e.g. 0.05 = 5%).
    pub min_split: f64,
    /// Number of function evaluations for the golden-section line search.
    pub line_search_evals: usize,
}

impl Default for PathFrankWolfeConfig {
    fn default() -> Self {
        Self { max_paths: 4, max_probe: 0.25, min_split: 0.05, line_search_evals: 12 }
    }
}

/// Split-routing algorithm that discovers multiple paths via Bellman-Ford
/// and optimises the input split across them using a Frank-Wolfe loop.
pub struct PathFrankWolfeAlgorithm {
    inner: BellmanFordAlgorithm,
    config: PathFrankWolfeConfig,
}

impl PathFrankWolfeAlgorithm {
    /// Creates a new `PathFrankWolfeAlgorithm`.
    pub(crate) fn new(algorithm_config: AlgorithmConfig, config: PathFrankWolfeConfig) -> Self {
        let inner = BellmanFordAlgorithm::with_config(algorithm_config);
        Self { inner, config }
    }
}

impl Default for PathFrankWolfeAlgorithm {
    fn default() -> Self {
        Self::new(AlgorithmConfig::default(), PathFrankWolfeConfig::default())
    }
}

impl PathFrankWolfeAlgorithm {
    /// Computes the minimum probe amount from the initial route's price impact.
    ///
    /// Returns `None` when the probe exceeds `config.max_probe × total_amount`,
    /// signalling that splitting is not worthwhile.
    fn compute_probe_amount(
        &self,
        total_amount: &BigUint,
        price_impact: f64,
        gas_cost_output_tokens: f64,
    ) -> Option<BigUint> {
        if price_impact <= 0.0 {
            return None;
        }
        let gas_floor = gas_cost_output_tokens / price_impact;

        let probe_amount = BigUint::from(gas_floor.ceil() as u128);
        let (max_probe_amount, _remainder) = split_amount(total_amount, self.config.max_probe);
        if probe_amount > max_probe_amount {
            return None;
        }

        Some(probe_amount)
    }

    /// Flow-fraction-weighted average price impact across all active paths.
    ///
    /// Per-path price impact measures how much the realized output falls short
    /// of the ideal (marginal-price) output. Paths are weighted by their share of
    /// total flow, not averaged equally — a 95/5 split means the big path
    /// dominates the result and the small path barely matters.
    fn compute_average_price_impact(paths: &[PathAllocation]) -> Result<f64, AlgorithmError> {
        let mut weighted_price_impact = 0.0;
        for path in paths {
            let first_hop = path
                .hops
                .first()
                .ok_or_else(|| AlgorithmError::Other("path has no hops".to_string()))?;
            let last_hop = path
                .hops
                .last()
                .ok_or_else(|| AlgorithmError::Other("path has no hops".to_string()))?;

            let amount_in = path.amount_in.to_f64().ok_or_else(|| {
                AlgorithmError::Other(format!("amount_in too large for f64: {}", path.amount_in))
            })?;
            let amount_out = path
                .amount_out
                .to_f64()
                .ok_or_else(|| {
                    AlgorithmError::Other(format!(
                        "amount_out too large for f64: {}",
                        path.amount_out
                    ))
                })?;
            if amount_in <= 0.0 {
                return Err(AlgorithmError::Other(format!("non-positive amount_in ({amount_in})")));
            }
            if path.marginal_price_product <= 0.0 {
                return Err(AlgorithmError::Other(format!(
                    "non-positive marginal_price_product ({})",
                    path.marginal_price_product
                )));
            }

            // marginal_price_product is in decimal-normalized units (from
            // spot_price), so convert raw amounts before comparing.
            let amount_in_decimal =
                amount_in / 10f64.powi(first_hop.descriptor.token_in.decimals as i32);
            let amount_out_decimal =
                amount_out / 10f64.powi(last_hop.descriptor.token_out.decimals as i32);
            let ideal_out = amount_in_decimal * path.marginal_price_product;
            let price_impact = 1.0 - amount_out_decimal / ideal_out;
            weighted_price_impact += path.flow_fraction * price_impact;
        }
        Ok(weighted_price_impact)
    }

    /// Finds the next candidate routing path for the Frank-Wolfe algorithm.
    ///
    /// Builds a post-swap market state that reflects `current_allocations` (applying each
    /// allocation's simulated pool outputs as overrides), then runs Bellman-Ford at
    /// `probe_amount` to discover the best remaining route.
    ///
    /// Pools already present in `current_allocations` are promoted to zero-gas so their
    /// committed gas cost is not counted again as marginal cost for the new path.
    ///
    /// Returns an ordered sequence of [`SimulatedHop`]s representing the discovered path,
    /// or an error if no route exists.
    pub(crate) fn find_candidate_path(
        &self,
        ctx: &BellmanFordContext,
        current_allocations: &[PathAllocation],
        probe_amount: &BigUint,
    ) -> Result<Vec<SimulatedHop>, AlgorithmError> {
        let mut overrides = build_post_swap_overrides(current_allocations, &ctx.market_data);

        // Pools committed in the current solution are executed once on-chain — their gas is
        // already priced into the combined transaction. Zero out protocol gas so BF doesn't
        // double-charge them when evaluating extensions. We track by (component_id, token_in,
        // token_out) because different token pairs through the same pool are separate on-chain
        // swaps with independent gas costs.
        for alloc in current_allocations {
            for hop in &alloc.hops {
                overrides = overrides.with_zero_gas(
                    hop.descriptor.component_id.clone(),
                    hop.descriptor.token_in.address.clone(),
                    hop.descriptor.token_out.address.clone(),
                );
            }
        }

        let token_in = ctx
            .node_address
            .get(&ctx.token_in_node)
            .cloned()
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token_in node index",
                id: Some(format!("{:?}", ctx.token_in_node)),
            })?;
        let token_out = ctx
            .node_address
            .get(&ctx.token_out_node)
            .cloned()
            .ok_or_else(|| AlgorithmError::DataNotFound {
                kind: "token_out node index",
                id: Some(format!("{:?}", ctx.token_out_node)),
            })?;
        let probe_order = Order::new(
            token_in,
            token_out,
            probe_amount.clone(),
            OrderSide::Sell,
            Default::default(),
        );

        let result =
            self.inner
                .find_single_route(ctx, &probe_order, FindRouteOptions { overrides })?;

        let route = result.route();
        let tokens = route.tokens();
        route
            .swaps()
            .iter()
            .map(|swap| {
                let token_in = tokens
                    .get(swap.token_in())
                    .cloned()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "token",
                        id: Some(format!("{:?}", swap.token_in())),
                    })?;
                let token_out = tokens
                    .get(swap.token_out())
                    .cloned()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "token",
                        id: Some(format!("{:?}", swap.token_out())),
                    })?;
                Ok(SimulatedHop {
                    descriptor: HopDescriptor::new(
                        swap.component_id().to_string(),
                        token_in,
                        token_out,
                    ),
                    amount_out: swap.amount_out().clone(),
                    gas: swap.gas_estimate().clone(),
                })
            })
            .collect()
    }

    /// Computes the gas cost of a route in output-token units as `f64`.
    fn gas_cost_output_tokens(
        route: &Route,
        ctx: &BellmanFordContext,
    ) -> Result<f64, AlgorithmError> {
        let gas_price = match &ctx.gas_price_wei {
            Some(gp) if !gp.is_zero() => gp,
            _ => return Ok(0.0),
        };
        let last_swap = route
            .swaps()
            .last()
            .ok_or_else(|| AlgorithmError::Other("route has no swaps".to_string()))?;
        let price = match ctx
            .token_prices
            .as_ref()
            .and_then(|tp| tp.get(last_swap.token_out()))
        {
            Some(p) if !p.denominator.is_zero() => p,
            _ => return Ok(0.0),
        };
        let gas_cost_wei = &route.total_gas() * gas_price;
        let gas_cost_tokens = &gas_cost_wei * &price.numerator / &price.denominator;
        Ok(gas_cost_tokens.to_f64().unwrap_or(0.0))
    }

    /// Converts a `Route` (from BF's initial solve) into a single `PathAllocation`.
    fn route_to_allocation(
        route: &Route,
        order: &Order,
        ctx: &BellmanFordContext,
    ) -> Result<PathAllocation, AlgorithmError> {
        let tokens = route.tokens();
        let hops: Vec<SimulatedHop> = route
            .swaps()
            .iter()
            .map(|swap| {
                let token_in = tokens
                    .get(swap.token_in())
                    .cloned()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "token",
                        id: Some(format!("{:?}", swap.token_in())),
                    })?;
                let token_out = tokens
                    .get(swap.token_out())
                    .cloned()
                    .ok_or_else(|| AlgorithmError::DataNotFound {
                        kind: "token",
                        id: Some(format!("{:?}", swap.token_out())),
                    })?;
                Ok(SimulatedHop {
                    descriptor: HopDescriptor::new(
                        swap.component_id().to_string(),
                        token_in,
                        token_out,
                    ),
                    amount_out: swap.amount_out().clone(),
                    gas: swap.gas_estimate().clone(),
                })
            })
            .collect::<Result<_, AlgorithmError>>()?;

        if hops.is_empty() {
            return Err(AlgorithmError::DataNotFound {
                kind: "swap",
                id: Some("route contains no swaps".to_string()),
            });
        }

        let descriptors: Vec<HopDescriptor> = hops
            .iter()
            .map(|h| h.descriptor.clone())
            .collect();
        let overrides = MarketOverrides::empty();
        let marginal_price_product =
            compute_marginal_price_product(&descriptors, &ctx.market_data, &overrides)?;
        let amount_out = hops
            .last()
            .map(|h| h.amount_out.clone())
            .unwrap_or_default();

        Ok(PathAllocation {
            hops,
            flow_fraction: 1.0,
            amount_in: order.amount().clone(),
            amount_out,
            marginal_price_product,
        })
    }

    /// Golden-section search over the step size `∈ [0, 1]`.
    ///
    /// At each probe point, builds trial fractions (existing paths scaled by
    /// `1 − step_size`, candidate at `step_size`) and evaluates the combined
    /// output via `evaluate_total_output`.
    fn optimize_step_size(
        &self,
        current_allocations: &[PathAllocation],
        candidate: &[SimulatedHop],
        total_amount: &BigUint,
        ctx: &BellmanFordContext,
    ) -> f64 {
        let existing_descriptors: Vec<Vec<HopDescriptor>> = current_allocations
            .iter()
            .map(|a| {
                a.hops
                    .iter()
                    .map(|h| h.descriptor.clone())
                    .collect()
            })
            .collect();
        let candidate_descriptors: Vec<HopDescriptor> = candidate
            .iter()
            .map(|h| h.descriptor.clone())
            .collect();
        let overrides = MarketOverrides::empty();

        // Evaluates the total output of the split that results from shifting
        // `step_size` fraction of flow from existing paths to the candidate.
        let evaluate_split = |step_size: f64| -> f64 {
            let mut trial_fractions: Vec<f64> = current_allocations
                .iter()
                .map(|a| a.flow_fraction * (1.0 - step_size))
                .collect();
            trial_fractions.push(step_size);

            let mut trial_paths: Vec<&[HopDescriptor]> = existing_descriptors
                .iter()
                .map(|v| v.as_slice())
                .collect();
            trial_paths.push(&candidate_descriptors);

            match evaluate_total_output(
                &trial_paths,
                &trial_fractions,
                total_amount,
                &ctx.market_data,
                &overrides,
            ) {
                Ok((total_output, _gas)) => total_output.to_f64().unwrap_or(0.0),
                Err(_) => 0.0,
            }
        };

        golden_section_search(evaluate_split, 0.0, 1.0, self.config.line_search_evals)
    }

    /// Applies a Frank-Wolfe step: shifts `step_size` fraction of flow to the
    /// candidate path, re-simulates all paths, and drops any path whose fraction
    /// falls below `config.min_split` (renormalizing the remainder).
    fn apply_step(
        &self,
        allocations: &mut Vec<PathAllocation>,
        candidate: &[SimulatedHop],
        step_size: f64,
        total_amount: &BigUint,
        ctx: &BellmanFordContext,
    ) -> Result<(), AlgorithmError> {
        for alloc in allocations.iter_mut() {
            alloc.flow_fraction *= 1.0 - step_size;
        }

        allocations.push(PathAllocation {
            hops: candidate.to_vec(),
            flow_fraction: step_size,
            amount_in: BigUint::zero(),
            amount_out: BigUint::zero(),
            marginal_price_product: 0.0,
        });

        allocations.retain(|a| a.flow_fraction >= self.config.min_split);

        let mut remaining_fractions: Vec<f64> = allocations
            .iter()
            .map(|a| a.flow_fraction)
            .collect();
        normalize_fractions(&mut remaining_fractions)
            .map_err(|e| AlgorithmError::Other(e.to_string()))?;
        let overrides = MarketOverrides::empty();
        for (alloc, &frac) in allocations
            .iter_mut()
            .zip(remaining_fractions.iter())
        {
            alloc.flow_fraction = frac;
            let (alloc_amount_in, _) = split_amount(total_amount, frac);
            let hop_descriptors: Vec<HopDescriptor> = alloc
                .hops
                .iter()
                .map(|h| h.descriptor.clone())
                .collect();
            let sim =
                simulate_path(&hop_descriptors, &alloc_amount_in, &ctx.market_data, &overrides)?;
            alloc.amount_in = alloc_amount_in;
            alloc.amount_out = sim.amount_out;
            alloc.marginal_price_product = sim.marginal_price_product;

            // Sync per-hop amount_out and gas with the final optimised split values so
            // build_split_route emits the correct swap amounts.
            for (hop, (amount_out, gas)) in alloc
                .hops
                .iter_mut()
                .zip(sim.hop_results)
            {
                hop.amount_out = amount_out;
                hop.gas = gas;
            }
        }

        Ok(())
    }

    /// Computes `net_amount_out` for a split route, mirroring
    /// `BellmanFordAlgorithm::compute_net_amount_out`.
    fn compute_split_net_amount_out(
        route: &Route,
        ctx: &BellmanFordContext,
    ) -> Result<BigInt, AlgorithmError> {
        let last_swap = route
            .swaps()
            .last()
            .ok_or_else(|| AlgorithmError::Other("route has no swaps".to_string()))?;
        let output_token = last_swap.token_out();
        let total_out: BigUint = route
            .swaps()
            .iter()
            .filter(|s| s.token_out() == output_token)
            .map(|s| s.amount_out().clone())
            .fold(BigUint::zero(), |acc, x| acc + x);

        let gas_cost = Self::gas_cost_output_tokens(route, ctx)?;
        let gas_cost_tokens = BigUint::from(gas_cost.ceil() as u128);
        Ok(BigInt::from(total_out) - BigInt::from(gas_cost_tokens))
    }

    /// Returns `true` if `candidate` has the same ordered sequence of
    /// `(component_id, token_in, token_out)` as any existing allocation.
    ///
    /// Both the pool and the token pair must match at every hop — the same pool used with
    /// different tokens (e.g. in a multi-token pool) is a distinct path.
    ///
    /// Paths that share only a prefix but diverge at a later hop are **not** duplicates — the
    /// shared hops are handled by `build_split_route`, which emits a single combined swap for
    /// the common segment.
    pub(crate) fn is_duplicate_path(
        candidate: &[SimulatedHop],
        existing: &[PathAllocation],
    ) -> bool {
        existing.iter().any(|alloc| {
            alloc.hops.len() == candidate.len() &&
                alloc
                    .hops
                    .iter()
                    .zip(candidate.iter())
                    .all(|(a, b)| {
                        a.descriptor.component_id == b.descriptor.component_id &&
                            a.descriptor.token_in.address == b.descriptor.token_in.address &&
                            a.descriptor.token_out.address == b.descriptor.token_out.address
                    })
        })
    }
}

impl Algorithm for PathFrankWolfeAlgorithm {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "path_frank_wolfe"
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
        let ctx = self
            .inner
            .build_context(graph, market, label, derived, order)
            .await?;

        // Step 1: initial single-path route via BF at full amount.
        let single_path_result =
            self.inner
                .find_single_route(&ctx, order, FindRouteOptions::default())?;

        let mut allocations =
            vec![Self::route_to_allocation(single_path_result.route(), order, &ctx)?];

        // Compute gas cost and initial probe.
        let gas_cost = Self::gas_cost_output_tokens(single_path_result.route(), &ctx)?;
        let total_amount = order.amount();
        let initial_pi = Self::compute_average_price_impact(&allocations)?;
        if self
            .compute_probe_amount(total_amount, initial_pi, gas_cost)
            .is_none()
        {
            debug!(pi = initial_pi, gas_cost, "price impact too low to justify splitting");
            return Ok(single_path_result);
        }

        // Step 2: Frank-Wolfe loop — discover up to max_paths - 1 additional paths.
        for iteration in 1..self.config.max_paths {
            if start.elapsed() >= self.timeout() {
                debug!(iteration, "pfw timeout, returning partial result");
                break;
            }

            let pi = Self::compute_average_price_impact(&allocations)?;
            let probe_amount = match self.compute_probe_amount(total_amount, pi, gas_cost) {
                Some(p) => p,
                None => {
                    debug!(iteration, pi, "probe exceeds cap, stopping");
                    break;
                }
            };

            let candidate = match self.find_candidate_path(&ctx, &allocations, &probe_amount) {
                Ok(c) => c,
                Err(e) => {
                    debug!(
                        iteration,
                        ?e,
                        "no additional candidate path found, stopping further searches"
                    );
                    break;
                }
            };

            if Self::is_duplicate_path(&candidate, &allocations) {
                debug!(iteration, "duplicate path, exploration exhausted");
                break;
            }

            // golden-section line search for optimal step size.
            let step_size = self.optimize_step_size(&allocations, &candidate, total_amount, &ctx);

            // step too small → no benefit.
            if step_size < self.config.min_split {
                debug!(iteration, step_size, "step size below min_split, stopping");
                break;
            }

            self.apply_step(&mut allocations, &candidate, step_size, total_amount, &ctx)?;
            debug!(iteration, paths = allocations.len(), step_size, "pfw iteration complete");
        }

        // Step 3: if we only have one path, the initial result is already optimal.
        if allocations.len() <= 1 {
            return Ok(single_path_result);
        }

        // Build the split route and compare with the initial single-path result.
        let split_route = build_split_route(&allocations, &ctx.market_data, order)?;
        let gas_price = ctx
            .gas_price_wei
            .clone()
            .unwrap_or_default();
        let split_net = Self::compute_split_net_amount_out(&split_route, &ctx)?;
        let split_result = RouteResult::new(split_route, split_net, gas_price);

        if split_result.net_amount_out() > single_path_result.net_amount_out() {
            debug!(
                split_net = %split_result.net_amount_out(),
                initial_net = %single_path_result.net_amount_out(),
                paths = allocations.len(),
                "split route beats single path"
            );
            Ok(split_result)
        } else {
            debug!(
                split_net = %split_result.net_amount_out(),
                initial_net = %single_path_result.net_amount_out(),
                "single path still best"
            );
            Ok(single_path_result)
        }
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("token_prices requirement conflicts (bug)")
            .allow_stale("spot_prices")
            .expect("spot_prices requirement conflicts (bug)")
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration as StdDuration};

    use tokio::sync::RwLock;
    use tycho_simulation::tycho_common::{
        models::token::Token,
        simulation::protocol_sim::{Price, ProtocolSim},
    };

    use super::*;
    use crate::{
        algorithm::{
            split_primitives::{build_split_route, MarketOverrides},
            test_utils::{
                order, setup_market_unweighted, token, token_with_decimals, ConstantProductSim,
                MockProtocolSim,
            },
            AlgorithmConfig,
        },
        derived::{types::TokenGasPrices, DerivedData, SharedDerivedDataRef},
        graph::GraphManager,
        types::OrderSide,
    };

    /// Creates a single dummy hop for test PathAllocations that need token
    /// decimal info but don't care about the actual path.
    fn dummy_hops(decimals_in: u32, decimals_out: u32) -> Vec<SimulatedHop> {
        vec![SimulatedHop {
            descriptor: HopDescriptor::new(
                "dummy".to_string(),
                token_with_decimals(0xAA, "IN", decimals_in),
                token_with_decimals(0xBB, "OUT", decimals_out),
            ),
            amount_out: BigUint::ZERO,
            gas: BigUint::ZERO,
        }]
    }

    /// Builds a `SharedDerivedDataRef` with token prices for the given tokens.
    ///
    /// Price is set so gas costs are small but non-zero relative to test trade
    /// amounts. With `setup_market_unweighted` (gas_price=100 wei) and pool
    /// gas=50,000, each hop costs ~5 output tokens.
    fn derived_with_token_prices(tokens: &[&Token]) -> SharedDerivedDataRef {
        let mut prices = TokenGasPrices::new();
        // 1 token = 1,000,000 wei of gas token.
        // gas_cost_tokens = (gas × gas_price) / 1,000,000
        //                 = (50,000 × 100) / 1,000,000 = 5 tokens per hop
        let price = Price::new(BigUint::from(1u64), BigUint::from(1_000_000u64));
        for token in tokens {
            prices.insert(token.address.clone(), price.clone());
        }
        let mut derived = DerivedData::new();
        derived.set_token_prices(prices, vec![], 1, true);
        Arc::new(RwLock::new(derived))
    }

    impl PathFrankWolfeAlgorithm {
        /// Returns a reference to the PFW-specific tuning config.
        fn pfw_config(&self) -> &PathFrankWolfeConfig {
            &self.config
        }
    }

    #[test]
    fn test_with_pfw_config_override() {
        let pfw_config = PathFrankWolfeConfig {
            max_paths: 8,
            max_probe: 0.5,
            min_split: 0.1,
            line_search_evals: 24,
        };
        let algo = PathFrankWolfeAlgorithm::new(AlgorithmConfig::default(), pfw_config);

        assert_eq!(algo.pfw_config().max_paths, 8);
        assert!((algo.pfw_config().max_probe - 0.5).abs() < f64::EPSILON);
        assert!((algo.pfw_config().min_split - 0.1).abs() < f64::EPSILON);
        assert_eq!(algo.pfw_config().line_search_evals, 24);
    }

    // ==================== compute_probe_amount ====================

    #[test]
    fn test_probe_amount_low_impact() {
        // Very small price impact makes gas_floor huge, exceeding max_probe cap → None.
        //   gas_floor = 100_000 / 0.001 = 100_000_000
        //   max_probe = 1_000_000 * 0.25 = 250_000
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::default();

        let result = algo.compute_probe_amount(&total, 0.001, 100_000.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_probe_amount_scaling() {
        // Higher price impact → lower probe floor (inversely proportional).
        //   probe = gas_cost / price_impact, so doubling price impact halves
        //   the probe.
        let total = BigUint::from(10_000_000u64);
        let algo = PathFrankWolfeAlgorithm::default();
        let gas_cost = 1000.0;

        let probe_high_pi = algo
            .compute_probe_amount(&total, 0.10, gas_cost)
            .unwrap();
        let probe_low_pi = algo
            .compute_probe_amount(&total, 0.05, gas_cost)
            .unwrap();

        assert!(probe_high_pi < probe_low_pi);

        // price_impact ratio is 0.10/0.05 = 2×, so the probe ratio should be
        // the inverse: 0.5×. Verify within 1% tolerance.
        let ratio = probe_high_pi.to_f64().unwrap() / probe_low_pi.to_f64().unwrap();
        assert!(
            (ratio - 0.5).abs() < 0.01,
            "expected ratio ~0.5 (inverse proportionality), got {ratio}"
        );
    }

    #[test]
    fn test_probe_amount_within_cap() {
        // Moderate price impact where probe fits within max_probe cap → Some.
        //   gas_floor = 1000 / 0.10 = 10_000
        //   max_probe = 1_000_000 * 0.25 = 250_000
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::default();

        let probe_amount = algo
            .compute_probe_amount(&total, 0.10, 1000.0)
            .unwrap();
        assert_eq!(probe_amount, BigUint::from(10_000u64));
    }

    #[test]
    fn test_probe_amount_zero_price_impact() {
        let total = BigUint::from(1_000_000u64);
        let algo = PathFrankWolfeAlgorithm::default();

        assert!(algo
            .compute_probe_amount(&total, 0.0, 1000.0)
            .is_none());
    }

    // ==================== compute_average_price_impact ====================

    #[test]
    fn test_average_price_impact_redistribution() {
        // Splitting flow across more paths should reduce average price impact.
        // Uses constant-product pool outputs (reserve_in=1M, reserve_out=2M) to construct
        // allocations at 1, 2, and 3 paths.
        let hops = dummy_hops(18, 18);
        let iter_0 = [PathAllocation {
            hops: hops.clone(),
            flow_fraction: 1.0,
            amount_in: BigUint::from(100_000u64),
            amount_out: BigUint::from(181_818u64),
            marginal_price_product: 2.0,
        }];

        let iter_1 = [
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: 0.5,
                amount_in: BigUint::from(50_000u64),
                amount_out: BigUint::from(95_238u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: 0.5,
                amount_in: BigUint::from(50_000u64),
                amount_out: BigUint::from(95_238u64),
                marginal_price_product: 2.0,
            },
        ];

        let third = 1.0 / 3.0;
        let iter_2 = [
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: third,
                amount_in: BigUint::from(33_333u64),
                amount_out: BigUint::from(64_514u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: third,
                amount_in: BigUint::from(33_333u64),
                amount_out: BigUint::from(64_514u64),
                marginal_price_product: 2.0,
            },
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: third,
                amount_in: BigUint::from(33_334u64),
                amount_out: BigUint::from(64_516u64),
                marginal_price_product: 2.0,
            },
        ];

        let pi_0 = PathFrankWolfeAlgorithm::compute_average_price_impact(&iter_0).unwrap();
        let pi_1 = PathFrankWolfeAlgorithm::compute_average_price_impact(&iter_1).unwrap();
        let pi_2 = PathFrankWolfeAlgorithm::compute_average_price_impact(&iter_2).unwrap();

        assert!(pi_1 < pi_0, "price impact should decrease after first split: {pi_1} >= {pi_0}");
        assert!(pi_2 < pi_1, "price impact should decrease after second split: {pi_2} >= {pi_1}");

        assert!((pi_0 - 0.09091).abs() < 1e-5, "expected ~0.0909, got {pi_0}");
        assert!((pi_1 - 0.04762).abs() < 1e-5, "expected ~0.0476, got {pi_1}");
        assert!((pi_2 - 0.03228).abs() < 1e-5, "expected ~0.0323, got {pi_2}");
    }

    #[test]
    fn test_average_price_impact_weighting() {
        // Weighted average: 90% of flow with 10% price impact + 10% of flow
        // with 50% price impact = 0.14, not the simple mean of 0.30.
        //   Path 1: flow=0.9, price_impact = 1 − 900/1000 = 0.10
        //   Path 2: flow=0.1, price_impact = 1 − 50/100  = 0.50
        //   Weighted = 0.9 × 0.10 + 0.1 × 0.50 = 0.14
        let hops = dummy_hops(18, 18);
        let allocations = [
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: 0.9,
                amount_in: BigUint::from(1000u64),
                amount_out: BigUint::from(900u64),
                marginal_price_product: 1.0,
            },
            PathAllocation {
                hops: hops.clone(),
                flow_fraction: 0.1,
                amount_in: BigUint::from(100u64),
                amount_out: BigUint::from(50u64),
                marginal_price_product: 1.0,
            },
        ];

        let pi = PathFrankWolfeAlgorithm::compute_average_price_impact(&allocations).unwrap();
        assert!((pi - 0.14).abs() < 1e-10, "expected 0.14, got {pi}");
    }

    #[test]
    fn test_average_price_impact_mixed_decimals() {
        // USDC (6 dec) → WETH (18 dec): spot_price = 0.0005 (human units).
        // Trade: 2000 USDC → ~1 WETH with 10% price impact.
        //   amount_in  = 2000 * 10^6  = 2_000_000_000 (raw USDC)
        //   amount_out = 0.9 * 10^18  = 900_000_000_000_000_000 (raw WETH)
        //   human_in   = 2000, human_out = 0.9
        //   ideal_out  = 2000 * 0.0005 = 1.0
        //   PI = 1 - 0.9/1.0 = 0.10
        let hops = dummy_hops(6, 18);
        let allocations = [PathAllocation {
            hops,
            flow_fraction: 1.0,
            amount_in: BigUint::from(2_000_000_000u64),
            amount_out: BigUint::from(900_000_000_000_000_000u64),
            marginal_price_product: 0.0005,
        }];

        let pi = PathFrankWolfeAlgorithm::compute_average_price_impact(&allocations).unwrap();
        assert!((pi - 0.10).abs() < 1e-10, "expected 0.10 for cross-decimal pair, got {pi}");
    }

    #[tokio::test]
    async fn test_pi_exit_criterion_with_high_gas() {
        // Three parallel pools, each A→B:
        //
        //        ┌──[P1]──┐
        //   A ───┼──[P2]──┼─── B
        //        └──[P3]──┘
        //
        // High gas costs relative to trade size mean that after the first split
        // lowers PI, `compute_probe_amount` returns None before iteration 2 can
        // discover the third pool → the loop exits via PI criterion at 2 swaps
        // instead of the 3 it would produce with lower gas.
        //
        // Math (constant-product, reserves R=5000, trade=2000):
        //   Initial PI (full amount, one pool): 2000/7000 ≈ 0.286
        //   After ~50/50 split (1000 each):     1000/6000 ≈ 0.167
        //   gas_cost = 1_000_000 × 100 / 1_000_000 = 100 output tokens
        //   PI threshold = gas_cost / (total × max_probe) = 100 / 500 = 0.2
        //   0.286 > 0.2 → enters loop; 0.167 < 0.2 → exits via PI criterion.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let cp = |gas: u64| -> Box<dyn ProtocolSim> {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(5_000u64),
                reserve_1: BigUint::from(5_000u64),
                gas,
            })
        };

        // High-gas run: PI exit should cap the result at 2 swaps.
        let (market_hi, gm_hi) = setup_market_unweighted(vec![
            ("P1", &token_a, &token_b, cp(1_000_000)),
            ("P2", &token_a, &token_b, cp(1_000_000)),
            ("P3", &token_a, &token_b, cp(1_000_000)),
        ]);

        let config = PathFrankWolfeConfig {
            max_paths: 4,
            max_probe: 0.25,
            min_split: 0.01,
            ..Default::default()
        };
        let algo = pfw_algo_with_config(2, config.clone());
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, 2_000, OrderSide::Sell);

        let result_hi = algo
            .find_best_route(gm_hi.graph(), market_hi, None, Some(derived.clone()), &ord)
            .await
            .unwrap();

        assert_eq!(
            result_hi.route().swaps().len(),
            2,
            "PI exit should stop the loop after the first split"
        );

        // Lower-gas control: same pools but gas_cost=50 output tokens.
        // PI threshold = 50 / 500 = 0.1, below post-split PI (~0.167),
        // so PI exit never fires and the algorithm discovers all three pools.
        let (market_lo, gm_lo) = setup_market_unweighted(vec![
            ("P1", &token_a, &token_b, cp(500_000)),
            ("P2", &token_a, &token_b, cp(500_000)),
            ("P3", &token_a, &token_b, cp(500_000)),
        ]);

        let algo_lo = pfw_algo_with_config(2, config);
        let result_lo = algo_lo
            .find_best_route(gm_lo.graph(), market_lo, None, Some(derived), &ord)
            .await
            .unwrap();

        assert_eq!(
            result_lo.route().swaps().len(),
            3,
            "without PI exit, all three pools should be used"
        );
    }

    // ==================== find_candidate_path / is_duplicate_path ====================

    fn pfw_algo(max_hops: usize) -> PathFrankWolfeAlgorithm {
        PathFrankWolfeAlgorithm::new(
            AlgorithmConfig::new(1, max_hops, StdDuration::from_millis(1000), None).unwrap(),
            PathFrankWolfeConfig::default(),
        )
    }

    #[test]
    fn test_is_duplicate_path_exact_match() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let candidate =
            vec![HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                .with_amounts(BigUint::from(200u64), BigUint::from(50_000u64))];
        let alloc = PathAllocation {
            hops: vec![HopDescriptor::new("P1".to_string(), token_a, token_b)
                .with_amounts(BigUint::from(200u64), BigUint::from(50_000u64))],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 2.0,
        };
        assert!(PathFrankWolfeAlgorithm::is_duplicate_path(&candidate, &[alloc]));
    }

    #[test]
    fn test_is_duplicate_path_shared_prefix() {
        // Existing: A──[P1]──B──[P2]──C
        // Candidate: A──[P1]──B──[P3]──C
        //
        // Shared first hop (P1) but divergent second hop → not a duplicate.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let zero = BigUint::from(0u64);
        let alloc = PathAllocation {
            hops: vec![
                HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                    .with_amounts(zero.clone(), zero.clone()),
                HopDescriptor::new("P2".to_string(), token_b.clone(), token_c.clone())
                    .with_amounts(zero.clone(), zero.clone()),
            ],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 1.0,
        };

        // [P1, P3] shares first hop with [P1, P2] but diverges at hop 2
        let candidate = vec![
            HopDescriptor::new("P1".to_string(), token_a, token_b.clone())
                .with_amounts(zero.clone(), zero.clone()),
            HopDescriptor::new("P3".to_string(), token_b, token_c)
                .with_amounts(zero.clone(), zero.clone()),
        ];
        assert!(!PathFrankWolfeAlgorithm::is_duplicate_path(&candidate, &[alloc]));
    }

    #[test]
    fn test_is_duplicate_path_same_pool_different_tokens() {
        // Existing:  A──[P1]──B
        // Candidate: A──[P1]──C
        //
        // Same pool but different output tokens → not a duplicate.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let zero = BigUint::from(0u64);
        let alloc = PathAllocation {
            hops: vec![HopDescriptor::new("P1".to_string(), token_a.clone(), token_b.clone())
                .with_amounts(zero.clone(), zero.clone())],
            flow_fraction: 1.0,
            amount_in: BigUint::from(100u64),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 2.0,
        };
        let candidate = vec![HopDescriptor::new("P1".to_string(), token_a, token_c)
            .with_amounts(zero.clone(), zero.clone())];
        assert!(!PathFrankWolfeAlgorithm::is_duplicate_path(&candidate, &[alloc]));
    }

    #[tokio::test]
    async fn test_shared_first_pool_two_outputs() {
        // Diamond topology — two paths share the entry pool:
        //
        //                ┌──[P2]──┐
        //   A ──[P1]── B ┤        ├ C
        //                └──[P3]──┘
        //
        // Path 1: A─[P1]─B─[P2]─C  (1.5× rate, degrades after first allocation)
        // Path 2: A─[P1]─B─[P3]─C  (1.0× rate, discovered second)
        //
        // Verifies: `is_duplicate_path` returns false, `build_split_route` emits 3 swaps,
        // P1 gas counted once.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, graph_manager) = setup_market_unweighted(vec![
            (
                "P1",
                &token_a,
                &token_b,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(10_000u64),
                    reserve_1: BigUint::from(10_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
            (
                "P2",
                &token_b,
                &token_c,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(1_000u64),
                    reserve_1: BigUint::from(1_500u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
            (
                "P3",
                &token_b,
                &token_c,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(1_000u64),
                    reserve_1: BigUint::from(1_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
        ]);

        let algo = pfw_algo(3);
        let probe_amount = BigUint::from(1_000u64);
        let ord = order(&token_a, &token_c, 1_000, OrderSide::Sell);

        let ctx = algo
            .inner
            .build_context(graph_manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // First candidate: P2 has 1.5x rate vs P3's 1.0x → finds [P1, P2].
        let first_path = algo
            .find_candidate_path(&ctx, &[], &probe_amount)
            .unwrap();
        assert_eq!(first_path[0].descriptor.component_id, "P1");
        assert_eq!(first_path[1].descriptor.component_id, "P2");

        let first_amount_out = first_path[1].amount_out.clone();
        let first_alloc = PathAllocation {
            hops: first_path,
            flow_fraction: 0.5,
            amount_in: probe_amount.clone(),
            amount_out: first_amount_out,
            marginal_price_product: 1.5,
        };

        // After allocating 1000 A on [P1, P2], P2 degrades enough that BF finds [P1, P3].
        let second_path = algo
            .find_candidate_path(&ctx, std::slice::from_ref(&first_alloc), &probe_amount)
            .unwrap();
        assert_eq!(second_path[0].descriptor.component_id, "P1");
        assert_eq!(second_path[1].descriptor.component_id, "P3");

        // Shared prefix [P1] does not make these duplicates.
        assert!(!PathFrankWolfeAlgorithm::is_duplicate_path(
            &second_path,
            std::slice::from_ref(&first_alloc)
        ));

        let second_amount_out = second_path[1].amount_out.clone();
        let second_alloc = PathAllocation {
            hops: second_path,
            flow_fraction: 0.5,
            amount_in: probe_amount.clone(),
            amount_out: second_amount_out,
            marginal_price_product: 1.0,
        };

        // build_split_route must emit 3 swaps: one combined A→B (P1), two B→C (P2, P3).
        let all_allocs = [first_alloc, second_alloc];
        let route = build_split_route(&all_allocs, &ctx.market_data, &ord).unwrap();
        let swaps = route.swaps();
        assert_eq!(swaps.len(), 3, "expected P1 + P2 + P3 = 3 swaps");
        let ids: Vec<&str> = swaps
            .iter()
            .map(|s| s.component_id())
            .collect();
        assert_eq!(
            ids.iter()
                .filter(|&&id| id == "P1")
                .count(),
            1,
            "P1 deduplicated"
        );
        assert!(ids.contains(&"P2"));
        assert!(ids.contains(&"P3"));
        // P1 gas counted once: P1(50k) + P2(50k) + P3(50k) = 150k.
        assert_eq!(route.total_gas(), BigUint::from(150_000u64));
    }

    #[tokio::test]
    async fn test_duplicate_path_stops_iteration() {
        // When BF repeatedly returns the same path, `is_duplicate_path` detects it so the
        // Frank-Wolfe loop can stop.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, graph_manager) = setup_market_unweighted(vec![(
            "P1",
            &token_a,
            &token_b,
            Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>,
        )]);

        let algo = pfw_algo(2);
        let probe_amount = BigUint::from(100u64);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let ctx = algo
            .inner
            .build_context(graph_manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        let first_path = algo
            .find_candidate_path(&ctx, &[], &probe_amount)
            .unwrap();
        assert_eq!(first_path[0].descriptor.component_id, "P1");

        let first_alloc = PathAllocation {
            hops: first_path,
            flow_fraction: 1.0,
            amount_in: probe_amount.clone(),
            amount_out: BigUint::from(200u64),
            marginal_price_product: 2.0,
        };

        // P1 is the only pool — BF returns it again.
        let second_path = algo
            .find_candidate_path(&ctx, std::slice::from_ref(&first_alloc), &probe_amount)
            .unwrap();
        assert!(PathFrankWolfeAlgorithm::is_duplicate_path(
            &second_path,
            std::slice::from_ref(&first_alloc)
        ));
    }

    #[test]
    fn test_with_zero_gas_zeroes_gas_keeps_amounts() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let sim = MockProtocolSim::new(2.0).with_gas(50_000);

        let overrides = MarketOverrides::empty()
            .with_override("P1".to_string(), Box::new(sim.clone()))
            .with_zero_gas("P1".to_string(), token_a.address.clone(), token_b.address.clone());

        let result = overrides
            .get(&"P1".to_string())
            .unwrap()
            .get_amount_out(BigUint::from(100u64), &token_a, &token_b)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(200u64), "amount unaffected");
        assert_eq!(result.gas, BigUint::ZERO, "gas zeroed by with_zero_gas");
    }

    // ==================== find_best_route main loop ====================

    fn pfw_algo_with_config(
        max_hops: usize,
        pfw_config: PathFrankWolfeConfig,
    ) -> PathFrankWolfeAlgorithm {
        PathFrankWolfeAlgorithm::new(
            AlgorithmConfig::new(1, max_hops, StdDuration::from_millis(5000), None).unwrap(),
            pfw_config,
        )
    }

    #[tokio::test]
    async fn test_single_path_no_split() {
        // Only one pool exists → the loop terminates via duplicate detection and
        // returns the single-path result unchanged.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, graph_manager) = setup_market_unweighted(vec![(
            "P1",
            &token_a,
            &token_b,
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(10_000u64),
                reserve_1: BigUint::from(10_000u64),
                gas: 50_000,
            }) as Box<dyn ProtocolSim>,
        )]);

        let algo = pfw_algo(2);
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, 1_000, OrderSide::Sell);

        let result = algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        let swaps = result.route().swaps();
        assert_eq!(swaps.len(), 1, "single path, single swap");
        assert_eq!(swaps[0].component_id(), "P1");
    }

    #[tokio::test]
    async fn test_two_parallel_pools_symmetric() {
        //        ┌──[P1]──┐
        //   A ───┤        ├─── B
        //        └──[P2]──┘
        //
        // Two identical pools → should split ~50/50.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let cp = |reserve: u64| -> Box<dyn ProtocolSim> {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve),
                reserve_1: BigUint::from(reserve),
                gas: 50_000,
            })
        };

        let (market, graph_manager) = setup_market_unweighted(vec![
            ("P1", &token_a, &token_b, cp(100_000)),
            ("P2", &token_a, &token_b, cp(100_000)),
        ]);

        let algo = pfw_algo_with_config(
            2,
            PathFrankWolfeConfig {
                max_paths: 4,
                max_probe: 0.25,
                min_split: 0.01,
                ..Default::default()
            },
        );
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, 10_000, OrderSide::Sell);

        let result = algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        let swaps = result.route().swaps();
        assert_eq!(swaps.len(), 2, "should use both pools");
        let ids: Vec<&str> = swaps
            .iter()
            .map(|s| s.component_id())
            .collect();
        assert!(ids.contains(&"P1"));
        assert!(ids.contains(&"P2"));

        // Both pools are identical → amounts should be roughly equal (within 10%).
        let amounts: Vec<f64> = swaps
            .iter()
            .map(|s| s.amount_in().to_f64().unwrap())
            .collect();
        let ratio = amounts[0] / amounts[1];
        assert!(
            (0.8..=1.2).contains(&ratio),
            "expected roughly equal split, got ratio {ratio} (amounts: {amounts:?})"
        );
    }

    #[tokio::test]
    async fn test_two_parallel_pools_asymmetric() {
        //        ┌──[deep: 200k]───┐
        //   A ───┤                  ├─── B
        //        └──[shallow: 50k]──┘
        //
        // Large trade should favor the deep pool but still use the shallow one.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let cp = |reserve: u64| -> Box<dyn ProtocolSim> {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve),
                reserve_1: BigUint::from(reserve),
                gas: 50_000,
            })
        };

        let (market, graph_manager) = setup_market_unweighted(vec![
            ("deep", &token_a, &token_b, cp(200_000)),
            ("shallow", &token_a, &token_b, cp(50_000)),
        ]);

        let algo = pfw_algo_with_config(
            2,
            PathFrankWolfeConfig {
                max_paths: 4,
                max_probe: 0.5,
                min_split: 0.01,
                ..Default::default()
            },
        );
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, 30_000, OrderSide::Sell);

        let result = algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        let swaps = result.route().swaps();
        assert_eq!(swaps.len(), 2, "should use both pools");

        let deep_swap = swaps
            .iter()
            .find(|s| s.component_id() == "deep")
            .unwrap();
        let shallow_swap = swaps
            .iter()
            .find(|s| s.component_id() == "shallow")
            .unwrap();

        assert!(
            deep_swap.amount_in() > shallow_swap.amount_in(),
            "deep pool should get more flow: deep={}, shallow={}",
            deep_swap.amount_in(),
            shallow_swap.amount_in()
        );
    }

    #[tokio::test]
    async fn test_split_vs_single_route() {
        //        ┌──[P1: 100k]──┐
        //   A ───┤              ├─── B
        //        └──[P2: 100k]──┘
        //
        // Large trade (50k) through two parallel pools should produce more
        // output than routing everything through just one.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let cp = |reserve: u64| -> Box<dyn ProtocolSim> {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve),
                reserve_1: BigUint::from(reserve),
                gas: 50_000,
            })
        };

        let (market, graph_manager) = setup_market_unweighted(vec![
            ("P1", &token_a, &token_b, cp(100_000)),
            ("P2", &token_a, &token_b, cp(100_000)),
        ]);

        let algo = pfw_algo_with_config(
            2,
            PathFrankWolfeConfig {
                max_paths: 4,
                max_probe: 0.25,
                min_split: 0.01,
                ..Default::default()
            },
        );
        // Large trade: 50% of each pool's reserves → significant price impact.
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, 50_000, OrderSide::Sell);

        let split_result = algo
            .find_best_route(
                graph_manager.graph(),
                market.clone(),
                None,
                Some(derived.clone()),
                &ord,
            )
            .await
            .unwrap();

        // Single-path: route everything through one pool.
        let single_algo =
            pfw_algo_with_config(2, PathFrankWolfeConfig { max_paths: 1, ..Default::default() });
        let single_result = single_algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        assert!(
            split_result.net_amount_out() > single_result.net_amount_out(),
            "split output ({}) should beat single ({})",
            split_result.net_amount_out(),
            single_result.net_amount_out()
        );
    }

    #[tokio::test]
    async fn test_three_paths_discovered() {
        //        ┌──[P1: 100k]──┐
        //   A ───┼──[P2:  80k]──┼─── B
        //        └──[P3:  60k]──┘
        //
        // Three parallel routes — with enough max_paths the algorithm
        // should discover all three.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let cp = |reserve: u64| -> Box<dyn ProtocolSim> {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve),
                reserve_1: BigUint::from(reserve),
                gas: 50_000,
            })
        };

        let (market, graph_manager) = setup_market_unweighted(vec![
            ("P1", &token_a, &token_b, cp(100_000)),
            ("P2", &token_a, &token_b, cp(80_000)),
            ("P3", &token_a, &token_b, cp(60_000)),
        ]);

        let algo = pfw_algo_with_config(
            2,
            PathFrankWolfeConfig {
                max_paths: 5,
                max_probe: 0.5,
                min_split: 0.01,
                line_search_evals: 16,
            },
        );
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let ord = order(&token_a, &token_b, 30_000, OrderSide::Sell);

        let result = algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        let swaps = result.route().swaps();
        let ids: Vec<&str> = swaps
            .iter()
            .map(|s| s.component_id())
            .collect();
        assert_eq!(ids.len(), 3, "expected 3 paths, got {ids:?}");
        assert!(ids.contains(&"P1"), "missing P1");
        assert!(ids.contains(&"P2"), "missing P2");
        assert!(ids.contains(&"P3"), "missing P3");
    }

    #[tokio::test]
    async fn test_shared_pool_degradation() {
        //        ┌──[P1]──┐
        //   A ───┤        ├─── B ──[P_shared]── C
        //        └──[P2]──┘
        //
        // Path 1: A─[P1]─B─[P_shared]─C
        // Path 2: A─[P2]─B─[P_shared]─C
        //
        // Both routes share the interior pool P_shared. Sequential simulation
        // through P_shared must degrade state correctly.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, graph_manager) = setup_market_unweighted(vec![
            (
                "P1",
                &token_a,
                &token_b,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(100_000u64),
                    reserve_1: BigUint::from(100_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
            (
                "P2",
                &token_a,
                &token_b,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(100_000u64),
                    reserve_1: BigUint::from(100_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
            (
                "P_shared",
                &token_b,
                &token_c,
                Box::new(ConstantProductSim {
                    reserve_0: BigUint::from(200_000u64),
                    reserve_1: BigUint::from(200_000u64),
                    gas: 50_000,
                }) as Box<dyn ProtocolSim>,
            ),
        ]);

        let algo = pfw_algo_with_config(
            3,
            PathFrankWolfeConfig {
                max_paths: 4,
                max_probe: 0.5,
                min_split: 0.01,
                ..Default::default()
            },
        );
        let derived = derived_with_token_prices(&[&token_a, &token_b, &token_c]);
        let ord = order(&token_a, &token_c, 20_000, OrderSide::Sell);

        let result = algo
            .find_best_route(
                graph_manager.graph(),
                market.clone(),
                None,
                Some(derived.clone()),
                &ord,
            )
            .await
            .unwrap();

        let swaps = result.route().swaps();
        let ids: Vec<&str> = swaps
            .iter()
            .map(|s| s.component_id())
            .collect();

        // Should use both entry pools plus the shared pool.
        assert!(ids.contains(&"P1") && ids.contains(&"P2"), "should use both entry pools");
        assert!(ids.contains(&"P_shared"), "must use shared B→C pool");

        // Output should be better than single-path (which goes through one entry
        // pool and hits P_shared with the full amount).
        let single_algo =
            pfw_algo_with_config(3, PathFrankWolfeConfig { max_paths: 1, ..Default::default() });
        let single_result = single_algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        assert!(
            result.net_amount_out() > single_result.net_amount_out(),
            "split ({}) should be > single ({})",
            result.net_amount_out(),
            single_result.net_amount_out()
        );
    }

    #[tokio::test]
    async fn test_timeout_mid_iteration() {
        //        ┌──[P0]──┐
        //        ├──[P1]──┤
        //   A ───┼──[P2]──┼─── B     (8 identical parallel pools)
        //        ├── ⋯  ──┤
        //        └──[P7]──┘
        //
        // With a generous timeout the algo splits across all pools.
        // With a near-zero timeout it returns fewer paths, proving the FW loop
        // was cut short while still producing a valid result.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let cp = |reserve: u64| -> Box<dyn ProtocolSim> {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(reserve),
                reserve_1: BigUint::from(reserve),
                gas: 50_000,
            })
        };

        let pool_names: [&str; 8] = ["P0", "P1", "P2", "P3", "P4", "P5", "P6", "P7"];

        let pfw_config =
            PathFrankWolfeConfig { max_paths: 8, min_split: 0.001, ..Default::default() };
        let ord = order(&token_a, &token_b, 80_000, OrderSide::Sell);

        // Generous timeout — should split across many pools.
        let pools: Vec<_> = pool_names
            .iter()
            .map(|id| (*id, &token_a, &token_b, cp(100_000)))
            .collect();
        let (market, graph_manager) = setup_market_unweighted(pools);
        let generous_algo = pfw_algo_with_config(2, pfw_config.clone());
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let generous_result = generous_algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();
        let generous_swaps = generous_result.route().swaps().len();

        // Near-zero timeout — should produce a valid result with fewer paths.
        let pools: Vec<_> = pool_names
            .iter()
            .map(|id| (*id, &token_a, &token_b, cp(100_000)))
            .collect();
        let (market, graph_manager) = setup_market_unweighted(pools);
        let timeout_algo = PathFrankWolfeAlgorithm::new(
            AlgorithmConfig::new(1, 2, StdDuration::from_millis(1), None).unwrap(),
            pfw_config,
        );
        let derived = derived_with_token_prices(&[&token_a, &token_b]);
        let timeout_result = timeout_algo
            .find_best_route(graph_manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();
        let timeout_swaps = timeout_result.route().swaps().len();

        assert!(
            !timeout_result
                .route()
                .swaps()
                .is_empty(),
            "timed-out result must still contain at least one swap"
        );
        assert!(
            timeout_swaps < generous_swaps,
            "timed-out result ({timeout_swaps} swaps) should use fewer paths \
             than generous result ({generous_swaps} swaps)"
        );
    }
}
