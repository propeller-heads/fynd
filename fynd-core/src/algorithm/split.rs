//! Split-routing algorithm.
//!
//! Single-path algorithms (MostLiquid, BellmanFord) send the whole order through one route. For
//! large orders, price impact makes it better to split the order across several routes so the
//! marginal price stays low. This algorithm:
//!
//! 1. Enumerates candidate paths (BFS, reusing `MostLiquidAlgorithm::find_paths`).
//! 2. Simulates each at the full amount and keeps the best single-path result (this alone matches
//!    or beats greedy Bellman-Ford because it re-simulates many candidates end-to-end).
//! 3. Keeps the existing pool-disjoint split route as the conservative fallback.
//! 4. Adds a shared-pool fill-and-spill candidate that commits chunks through shared pool state.
//! 5. Returns whichever is better: best single, disjoint split, or shared split.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use petgraph::{
    graph::{EdgeIndex, NodeIndex},
    prelude::EdgeRef,
};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim, tycho_core::models::Address,
};

use super::{
    most_liquid::DepthAndPrice,
    split_primitives::{build_split_route, HopDescriptor, PathAllocation, SimulatedHop},
    Algorithm, AlgorithmConfig, MostLiquidAlgorithm, NoPathReason,
};
use crate::{
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, Path, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, RouteResult},
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
/// Number of top full-amount paths always considered for shared-pool splitting.
const SHARED_FULL_PATHS: usize = 16;
/// Number of heuristic-ranked paths probed with the first shared-pool chunk.
const SHARED_MARGIN_PROBE_PATHS: usize = 96;
/// Number of marginal-probe winners added to the shared-pool candidate set.
const SHARED_MARGIN_PATHS: usize = 12;
/// Upper bound on shared-pool candidate paths.
const SHARED_MAX_CANDIDATES: usize = 24;
/// Upper bound on active paths in the shared-pool allocation.
const SHARED_MAX_ACTIVE_PATHS: usize = 8;
/// Number of chunks for shared-pool fill-and-spill.
const SHARED_NUM_CHUNKS: usize = 64;

type PoolStateUpdates = Vec<(ComponentId, Box<dyn ProtocolSim>)>;
type SharedProbe = (BigUint, BigUint, PoolStateUpdates);

struct SplitEvalContext<'a> {
    market: &'a MarketState,
    amount_in: &'a BigUint,
    gas_price: &'a BigUint,
    token_prices: Option<&'a TokenGasPrices>,
    token_out: &'a Address,
    start: &'a Instant,
    timeout_ms: u64,
}

impl SplitEvalContext<'_> {
    fn timed_out(&self) -> bool {
        self.start.elapsed().as_millis() as u64 > self.timeout_ms
    }
}

struct FullPathResults {
    best_single: RouteResult,
    full_outputs: Vec<(usize, BigUint)>,
}

/// Routes orders by splitting them across multiple paths to minimize price impact.
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

    fn ranked_paths<'a>(
        graph: &'a StableDiGraph<DepthAndPrice>,
        order: &Order,
        min_hops: usize,
        max_hops: usize,
        max_candidates: usize,
        connector_tokens: Option<&HashSet<Address>>,
    ) -> Result<Vec<Path<'a, DepthAndPrice>>, AlgorithmError> {
        let all_paths = MostLiquidAlgorithm::find_paths(
            graph,
            order.token_in(),
            order.token_out(),
            min_hops,
            max_hops,
            connector_tokens,
        )?;
        if all_paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        let mut scored: Vec<(Path<DepthAndPrice>, f64)> = all_paths
            .into_iter()
            .map(|path| {
                let score = MostLiquidAlgorithm::try_score_path(&path).unwrap_or(f64::MIN);
                (path, score)
            })
            .collect();
        scored.sort_by(|(_, a), (_, b)| {
            b.partial_cmp(a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(max_candidates);
        Ok(scored
            .into_iter()
            .map(|(path, _)| path)
            .collect())
    }

    async fn token_prices_from(derived: Option<&SharedDerivedDataRef>) -> Option<TokenGasPrices> {
        match derived {
            Some(derived) => derived
                .read()
                .await
                .token_prices()
                .cloned(),
            None => None,
        }
    }

    async fn market_subset(
        market: &MarketData,
        label: Option<&StateLabel>,
        component_ids: &HashSet<ComponentId>,
    ) -> Result<MarketState, AlgorithmError> {
        let view = match label {
            Some(label) => market
                .read_labeled(label)
                .await
                .map_err(|err| AlgorithmError::Other(err.to_string()))?,
            None => market.read().await,
        };
        if view.gas_price().is_none() {
            return Err(AlgorithmError::DataNotFound { kind: "gas price", id: None });
        }
        Ok(view.extract_subset_with_overlay(component_ids))
    }

    fn simulate_full_paths(
        paths: &[Path<DepthAndPrice>],
        ctx: &SplitEvalContext<'_>,
    ) -> Option<FullPathResults> {
        let mut best_single: Option<RouteResult> = None;
        let mut full_outputs = Vec::new();

        for (idx, path) in paths.iter().enumerate() {
            if ctx.timed_out() {
                break;
            }
            let Ok(result) = MostLiquidAlgorithm::simulate_path(
                path,
                ctx.market,
                ctx.token_prices,
                ctx.amount_in.clone(),
            ) else {
                continue;
            };
            let gross = result
                .route()
                .swaps()
                .last()
                .map(|swap| swap.amount_out().clone())
                .unwrap_or_else(BigUint::zero);
            full_outputs.push((idx, gross));
            if best_single
                .as_ref()
                .map(|best| result.net_amount_out() > best.net_amount_out())
                .unwrap_or(true)
            {
                best_single = Some(result);
            }
        }

        full_outputs.sort_by(|(_, a), (_, b)| b.cmp(a));
        Some(FullPathResults { best_single: best_single?, full_outputs })
    }

    fn choose_best_route(
        best_single: RouteResult,
        candidates: [Option<RouteResult>; 2],
    ) -> RouteResult {
        let mut best_net = best_single.net_amount_out().clone();
        let mut best_route = None;
        for candidate in candidates.into_iter().flatten() {
            if candidate.net_amount_out() > &best_net {
                best_net = candidate.net_amount_out().clone();
                best_route = Some(candidate);
            }
        }

        best_route.unwrap_or(best_single)
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
        let amount_in = amount;
        let mut current = amount_in.clone();
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

    fn chunks(amount: &BigUint, count: usize) -> Vec<BigUint> {
        let count = count.max(1);
        let base = amount / count;
        if base.is_zero() {
            return Vec::new();
        }
        let remainder = amount - &base * count;
        let mut chunks = Vec::with_capacity(count);
        chunks.push(&base + &remainder);
        for _ in 1..count {
            chunks.push(base.clone());
        }
        chunks
    }

    fn combined_net(
        ctx: &SplitEvalContext<'_>,
        total_gross: BigUint,
        total_gas: &BigUint,
    ) -> BigInt {
        match Self::gas_cost_in_token(total_gas, ctx.gas_price, ctx.token_prices, ctx.token_out) {
            Some(cost) => BigInt::from(total_gross) - BigInt::from(cost),
            None => BigInt::from(total_gross),
        }
    }

    fn simulate_on_overrides(
        path: &Path<DepthAndPrice>,
        ctx: &SplitEvalContext<'_>,
        overrides: &HashMap<ComponentId, Box<dyn ProtocolSim>>,
        amount: BigUint,
    ) -> Option<SharedProbe> {
        let mut current = amount;
        let mut total_gas = BigUint::zero();
        let mut local: HashMap<&ComponentId, Box<dyn ProtocolSim>> = HashMap::new();

        for (address_in, edge, address_out) in path.iter() {
            let token_in = ctx.market.get_token(address_in)?;
            let token_out = ctx.market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let state: &dyn ProtocolSim = if let Some(state) = local.get(component_id) {
                state.as_ref()
            } else if let Some(state) = overrides.get(component_id) {
                state.as_ref()
            } else {
                ctx.market
                    .get_simulation_state(component_id)?
            };
            let result = state
                .get_amount_out(current.clone(), token_in, token_out)
                .ok()?;
            total_gas += &result.gas;
            local.insert(component_id, result.new_state);
            current = result.amount;
        }

        let updates = local
            .into_iter()
            .map(|(id, state)| (id.clone(), state))
            .collect();
        Some((current, total_gas, updates))
    }

    fn simulate_allocation_commit(
        path: &Path<DepthAndPrice>,
        ctx: &SplitEvalContext<'_>,
        overrides: &mut HashMap<ComponentId, Box<dyn ProtocolSim>>,
        amount: BigUint,
        flow_fraction: f64,
    ) -> Option<PathAllocation> {
        let amount_in = amount;
        let mut current = amount_in.clone();
        let mut hops = Vec::with_capacity(path.len());

        for (address_in, edge, address_out) in path.iter() {
            let token_in = ctx.market.get_token(address_in)?;
            let token_out = ctx.market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let state = overrides
                .get(component_id)
                .map(Box::as_ref)
                .or_else(|| {
                    ctx.market
                        .get_simulation_state(component_id)
                })?;
            let result = state
                .get_amount_out(current.clone(), token_in, token_out)
                .ok()?;
            hops.push(SimulatedHop {
                descriptor: HopDescriptor::new(
                    component_id.clone(),
                    token_in.clone(),
                    token_out.clone(),
                ),
                amount_out: result.amount.clone(),
                gas: result.gas.clone(),
            });
            overrides.insert(component_id.clone(), result.new_state);
            current = result.amount;
        }

        Some(PathAllocation {
            hops,
            flow_fraction,
            amount_in,
            amount_out: current,
            marginal_price_product: 0.0,
        })
    }

    fn push_unique(indices: &mut Vec<usize>, idx: usize) {
        if !indices.contains(&idx) {
            indices.push(idx);
        }
    }

    fn component_ids_for_paths(paths: &[Path<DepthAndPrice>]) -> HashSet<ComponentId> {
        paths
            .iter()
            .flat_map(|path| {
                path.edge_iter()
                    .iter()
                    .map(|edge| edge.component_id.clone())
            })
            .collect()
    }

    fn collect_reachable_components(
        graph: &StableDiGraph<DepthAndPrice>,
        token_in: &Address,
        max_hops: usize,
        out: &mut HashSet<ComponentId>,
    ) {
        let Some(from_idx) = Self::find_node(graph, token_in) else {
            return;
        };
        let mut visited = HashSet::from([from_idx]);
        let mut queue = VecDeque::from([(from_idx, 0usize)]);
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            for edge in graph.edges(node) {
                out.insert(edge.weight().component_id.clone());
                let target = edge.target();
                if visited.insert(target) {
                    queue.push_back((target, depth + 1));
                }
            }
        }
    }

    fn append_bellman_ford_rescues<'a>(
        graph: &'a StableDiGraph<DepthAndPrice>,
        paths: &mut Vec<Path<'a, DepthAndPrice>>,
        ctx: &SplitEvalContext<'_>,
        order: &Order,
        max_hops: usize,
        connector_tokens: Option<&HashSet<Address>>,
    ) {
        let bf_paths =
            Self::bellman_ford_paths(graph, ctx, order.token_in(), max_hops, connector_tokens);
        if bf_paths.is_empty() {
            return;
        }

        let mut seen: HashSet<Vec<(Address, ComponentId, Address)>> = paths
            .iter()
            .map(Self::path_key)
            .collect();
        for path in bf_paths {
            if seen.insert(Self::path_key(&path)) {
                paths.push(path);
            }
        }
    }

    fn select_shared_candidates(
        paths: &[Path<DepthAndPrice>],
        full_outputs: &[(usize, BigUint)],
        first_chunk: &BigUint,
        ctx: &SplitEvalContext<'_>,
    ) -> Vec<usize> {
        let mut candidates = Vec::with_capacity(SHARED_MAX_CANDIDATES);
        for (idx, _) in full_outputs
            .iter()
            .take(SHARED_FULL_PATHS)
        {
            Self::push_unique(&mut candidates, *idx);
        }

        let empty_overrides = HashMap::new();
        let mut marginal = Vec::new();
        for (idx, path) in paths
            .iter()
            .enumerate()
            .take(SHARED_MARGIN_PROBE_PATHS)
        {
            if ctx.timed_out() {
                break;
            }
            let Some((out, gas, _)) =
                Self::simulate_on_overrides(path, ctx, &empty_overrides, first_chunk.clone())
            else {
                continue;
            };
            let activation =
                Self::gas_cost_in_token(&gas, ctx.gas_price, ctx.token_prices, ctx.token_out)
                    .map(BigInt::from)
                    .unwrap_or_else(BigInt::zero);
            marginal.push((idx, BigInt::from(out) - activation));
        }
        marginal.sort_by(|(_, a), (_, b)| b.cmp(a));

        for (idx, net) in marginal
            .into_iter()
            .take(SHARED_MARGIN_PATHS)
        {
            if net <= BigInt::zero() {
                continue;
            }
            Self::push_unique(&mut candidates, idx);
            if candidates.len() >= SHARED_MAX_CANDIDATES {
                break;
            }
        }
        candidates
    }

    fn build_disjoint_route(
        paths: &[Path<DepthAndPrice>],
        selected: &[usize],
        ctx: &SplitEvalContext<'_>,
        order: &Order,
        num_chunks: usize,
    ) -> Option<RouteResult> {
        if selected.len() < 2 {
            return None;
        }

        let chunks = Self::chunks(ctx.amount_in, num_chunks);
        if chunks.is_empty() {
            return None;
        }

        let mut alloc = vec![BigUint::zero(); selected.len()];
        let mut cur_out = vec![BigUint::zero(); selected.len()];
        let mut used = vec![false; selected.len()];

        for chunk in chunks {
            if ctx.timed_out() {
                break;
            }
            let mut best: Option<(usize, BigInt, BigUint)> = None;
            for (i, &path_idx) in selected.iter().enumerate() {
                let probe_in = &alloc[i] + &chunk;
                let Some((probe_out, probe_gas)) =
                    Self::simulate_amount(&paths[path_idx], ctx.market, probe_in)
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
                        ctx.gas_price,
                        ctx.token_prices,
                        ctx.token_out,
                    )
                    .map(BigInt::from)
                    .unwrap_or_else(BigInt::zero);
                    gross_marginal - activation
                };
                if best
                    .as_ref()
                    .map(|(_, best_net, _)| &net_marginal > best_net)
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

        Self::assemble_disjoint_route(paths, selected, &alloc, ctx, order)
    }

    fn assemble_disjoint_route(
        paths: &[Path<DepthAndPrice>],
        selected: &[usize],
        alloc: &[BigUint],
        ctx: &SplitEvalContext<'_>,
        order: &Order,
    ) -> Option<RouteResult> {
        let mut allocations = Vec::new();

        for (i, &path_idx) in selected.iter().enumerate() {
            if alloc[i].is_zero() {
                continue;
            }
            let allocation = Self::path_allocation(&paths[path_idx], ctx, alloc[i].clone())?;
            allocations.push(allocation);
        }

        if allocations.len() < 2 {
            return None;
        }
        Self::route_result_from_allocations(&allocations, ctx, order)
    }

    fn fill_and_spill(
        paths: &[Path<DepthAndPrice>],
        full_outputs: &[(usize, BigUint)],
        ctx: &SplitEvalContext<'_>,
        order: &Order,
    ) -> Option<RouteResult> {
        let chunks = Self::chunks(ctx.amount_in, SHARED_NUM_CHUNKS);
        if chunks.is_empty() {
            return None;
        }
        let candidates = Self::select_shared_candidates(paths, full_outputs, &chunks[0], ctx);
        if candidates.len() < 2 {
            return None;
        }

        let mut alloc = vec![BigUint::zero(); candidates.len()];
        let mut active = vec![false; candidates.len()];
        let mut active_count = 0usize;
        let mut overrides: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();

        for chunk in &chunks {
            if ctx.timed_out() {
                break;
            }
            let mut best: Option<(usize, BigInt, PoolStateUpdates)> = None;
            for (i, &path_idx) in candidates.iter().enumerate() {
                if !active[i] && active_count >= SHARED_MAX_ACTIVE_PATHS {
                    continue;
                }
                let Some((out, gas, updates)) =
                    Self::simulate_on_overrides(&paths[path_idx], ctx, &overrides, chunk.clone())
                else {
                    continue;
                };
                let net_marginal = if active[i] {
                    BigInt::from(out)
                } else {
                    let activation = Self::gas_cost_in_token(
                        &gas,
                        ctx.gas_price,
                        ctx.token_prices,
                        ctx.token_out,
                    )
                    .map(BigInt::from)
                    .unwrap_or_else(BigInt::zero);
                    BigInt::from(out) - activation
                };
                if best
                    .as_ref()
                    .map(|(_, best_net, _)| &net_marginal > best_net)
                    .unwrap_or(true)
                {
                    best = Some((i, net_marginal, updates));
                }
            }

            let Some((best_i, best_net, updates)) = best else {
                break;
            };
            if !active[best_i] && best_net <= BigInt::zero() {
                break;
            }
            alloc[best_i] += chunk;
            if !active[best_i] {
                active[best_i] = true;
                active_count += 1;
            }
            for (component_id, state) in updates {
                overrides.insert(component_id, state);
            }
        }

        if active_count < 2 {
            return None;
        }
        let allocated: BigUint = alloc.iter().sum();
        if &allocated < ctx.amount_in {
            let leftover = ctx.amount_in - &allocated;
            let best_i = (0..alloc.len())
                .filter(|&i| active[i])
                .max_by(|&a, &b| alloc[a].cmp(&alloc[b]))?;
            alloc[best_i] += leftover;
        }

        Self::assemble_shared_route(paths, &candidates, &alloc, ctx, order)
    }

    fn assemble_shared_route(
        paths: &[Path<DepthAndPrice>],
        selected: &[usize],
        alloc: &[BigUint],
        ctx: &SplitEvalContext<'_>,
        order: &Order,
    ) -> Option<RouteResult> {
        let mut execution_order: Vec<usize> = (0..selected.len())
            .filter(|&i| !alloc[i].is_zero())
            .collect();
        if execution_order.len() < 2 {
            return None;
        }
        execution_order.sort_by(|&a, &b| alloc[b].cmp(&alloc[a]));

        let mut overrides: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut allocations = Vec::new();

        for i in execution_order {
            let path_idx = selected[i];
            let split_fraction = ratio(&alloc[i], ctx.amount_in);
            let allocation = Self::simulate_allocation_commit(
                &paths[path_idx],
                ctx,
                &mut overrides,
                alloc[i].clone(),
                split_fraction,
            )?;
            allocations.push(allocation);
        }

        if allocations.len() < 2 {
            return None;
        }
        Self::route_result_from_allocations(&allocations, ctx, order)
    }

    fn path_allocation(
        path: &Path<DepthAndPrice>,
        ctx: &SplitEvalContext<'_>,
        amount: BigUint,
    ) -> Option<PathAllocation> {
        let flow_fraction = ratio(&amount, ctx.amount_in);
        let mut overrides = HashMap::new();
        Self::simulate_allocation_commit(path, ctx, &mut overrides, amount, flow_fraction)
    }

    fn route_result_from_allocations(
        allocations: &[PathAllocation],
        ctx: &SplitEvalContext<'_>,
        order: &Order,
    ) -> Option<RouteResult> {
        let route = build_split_route(allocations, ctx.market, order).ok()?;
        let total_gross = route
            .swaps()
            .iter()
            .filter(|swap| swap.token_out() == ctx.token_out)
            .fold(BigUint::zero(), |acc, swap| acc + swap.amount_out());
        if total_gross.is_zero() {
            return None;
        }
        let total_gas = route.total_gas();
        let net = Self::combined_net(ctx, total_gross, &total_gas);
        Some(RouteResult::new(route, net, ctx.gas_price.clone()))
    }

    fn bellman_ford_paths<'a>(
        graph: &'a StableDiGraph<DepthAndPrice>,
        ctx: &SplitEvalContext<'_>,
        token_in: &Address,
        max_hops: usize,
        connector_tokens: Option<&HashSet<Address>>,
    ) -> Vec<Path<'a, DepthAndPrice>> {
        let (Some(from_idx), Some(to_idx)) =
            (Self::find_node(graph, token_in), Self::find_node(graph, ctx.token_out))
        else {
            return Vec::new();
        };
        if from_idx == to_idx {
            return Vec::new();
        }

        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        for gas_aware in [true, false] {
            let Some(path) = Self::spfa_best_path(
                graph,
                ctx,
                from_idx,
                to_idx,
                max_hops,
                gas_aware,
                connector_tokens,
            ) else {
                continue;
            };
            if seen.insert(Self::path_key(&path)) {
                paths.push(path);
            }
        }
        paths
    }

    fn find_node(graph: &StableDiGraph<DepthAndPrice>, token: &Address) -> Option<NodeIndex> {
        graph
            .node_indices()
            .find(|&node| &graph[node] == token)
    }

    fn path_key(path: &Path<DepthAndPrice>) -> Vec<(Address, ComponentId, Address)> {
        path.iter()
            .map(|(from, edge, to)| (from.clone(), edge.component_id.clone(), to.clone()))
            .collect()
    }

    fn spfa_best_path<'a>(
        graph: &'a StableDiGraph<DepthAndPrice>,
        ctx: &SplitEvalContext<'_>,
        from_idx: NodeIndex,
        to_idx: NodeIndex,
        max_hops: usize,
        gas_aware: bool,
        connector_tokens: Option<&HashSet<Address>>,
    ) -> Option<Path<'a, DepthAndPrice>> {
        let max_idx = graph
            .node_indices()
            .map(|node| node.index())
            .max()? +
            1;
        let mut amount = vec![BigUint::zero(); max_idx];
        let mut cumul_gas = vec![BigUint::zero(); max_idx];
        let mut predecessor: Vec<Option<(NodeIndex, EdgeIndex)>> = vec![None; max_idx];
        amount[from_idx.index()] = ctx.amount_in.clone();

        let mut active = vec![from_idx];
        for _ in 0..max_hops {
            if ctx.timed_out() || active.is_empty() {
                break;
            }
            let mut next = HashSet::new();
            for &node in &active {
                let node_idx = node.index();
                if amount[node_idx].is_zero() {
                    continue;
                }
                let Some(token_in) = ctx.market.get_token(&graph[node]) else {
                    continue;
                };
                for edge in graph.edges(node) {
                    let target = edge.target();
                    let component_id = &edge.weight().component_id;
                    if Self::spfa_conflicts(node, target, component_id, &predecessor, graph) {
                        continue;
                    }
                    if let Some(tokens) = connector_tokens {
                        let address = &graph[target];
                        if target != to_idx && target != from_idx && !tokens.contains(address) {
                            continue;
                        }
                    }
                    let Some(token_out) = ctx.market.get_token(&graph[target]) else {
                        continue;
                    };
                    let Some(state) = ctx
                        .market
                        .get_simulation_state(component_id)
                    else {
                        continue;
                    };
                    let Ok(result) =
                        state.get_amount_out(amount[node_idx].clone(), token_in, token_out)
                    else {
                        continue;
                    };
                    let candidate_gas = &cumul_gas[node_idx] + &result.gas;
                    let target_idx = target.index();
                    if Self::spfa_is_better(
                        &result.amount,
                        &candidate_gas,
                        &amount[target_idx],
                        &cumul_gas[target_idx],
                        &graph[target],
                        gas_aware,
                        ctx,
                    ) {
                        amount[target_idx] = result.amount;
                        cumul_gas[target_idx] = candidate_gas;
                        predecessor[target_idx] = Some((node, edge.id()));
                        next.insert(target);
                    }
                }
            }
            active = next.into_iter().collect();
            active.sort_unstable();
        }

        if amount[to_idx.index()].is_zero() {
            return None;
        }
        Self::reconstruct_spfa_path(graph, from_idx, to_idx, &predecessor)
    }

    fn spfa_is_better(
        candidate_amount: &BigUint,
        candidate_gas: &BigUint,
        existing_amount: &BigUint,
        existing_gas: &BigUint,
        token_out: &Address,
        gas_aware: bool,
        ctx: &SplitEvalContext<'_>,
    ) -> bool {
        if existing_amount.is_zero() {
            return true;
        }
        if gas_aware {
            let candidate = Self::spfa_net(candidate_amount, candidate_gas, token_out, ctx);
            let existing = Self::spfa_net(existing_amount, existing_gas, token_out, ctx);
            candidate > existing
        } else {
            candidate_amount > existing_amount
        }
    }

    fn spfa_net(
        gross: &BigUint,
        gas: &BigUint,
        token_out: &Address,
        ctx: &SplitEvalContext<'_>,
    ) -> BigInt {
        match Self::gas_cost_in_token(gas, ctx.gas_price, ctx.token_prices, token_out) {
            Some(cost) => BigInt::from(gross.clone()) - BigInt::from(cost),
            None => BigInt::from(gross.clone()),
        }
    }

    fn spfa_conflicts(
        from: NodeIndex,
        target_node: NodeIndex,
        target_pool: &ComponentId,
        predecessor: &[Option<(NodeIndex, EdgeIndex)>],
        graph: &StableDiGraph<DepthAndPrice>,
    ) -> bool {
        let mut current = from;
        loop {
            if current == target_node {
                return true;
            }
            match predecessor[current.index()] {
                Some((prev, edge_idx)) => {
                    if graph
                        .edge_weight(edge_idx)
                        .map(|weight| &weight.component_id == target_pool)
                        .unwrap_or(false)
                    {
                        return true;
                    }
                    current = prev;
                }
                None => return false,
            }
        }
    }

    fn reconstruct_spfa_path<'a>(
        graph: &'a StableDiGraph<DepthAndPrice>,
        from_idx: NodeIndex,
        to_idx: NodeIndex,
        predecessor: &[Option<(NodeIndex, EdgeIndex)>],
    ) -> Option<Path<'a, DepthAndPrice>> {
        let mut hops = Vec::new();
        let mut current = to_idx;
        let mut visited = HashSet::new();
        while current != from_idx {
            if !visited.insert(current) {
                return None;
            }
            let (prev, edge_idx) = predecessor[current.index()]?;
            hops.push((prev, edge_idx, current));
            current = prev;
        }
        hops.reverse();

        let mut path = Path::new();
        for (prev, edge_idx, current) in hops {
            let edge = graph.edge_weight(edge_idx)?;
            path.add_hop(&graph[prev], edge, &graph[current]);
        }
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
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

        let token_prices = Self::token_prices_from(derived.as_ref()).await;
        let amount_in = order.amount().clone();
        let mut paths = Self::ranked_paths(
            graph,
            order,
            self.min_hops,
            self.max_hops,
            self.max_candidates,
            self.connector_tokens.as_ref(),
        )?;
        let split_path_count = paths.len();

        let mut component_ids = Self::component_ids_for_paths(&paths);
        Self::collect_reachable_components(
            graph,
            order.token_in(),
            self.max_hops,
            &mut component_ids,
        );
        let market = Self::market_subset(&market, label.as_ref(), &component_ids).await?;
        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();
        let timeout_ms = self.timeout.as_millis() as u64;
        let ctx = SplitEvalContext {
            market: &market,
            amount_in: &amount_in,
            gas_price: &gas_price,
            token_prices: token_prices.as_ref(),
            token_out: order.token_out(),
            start: &start,
            timeout_ms,
        };
        Self::append_bellman_ford_rescues(
            graph,
            &mut paths,
            &ctx,
            order,
            self.max_hops,
            self.connector_tokens.as_ref(),
        );

        let FullPathResults { best_single, full_outputs } =
            Self::simulate_full_paths(&paths, &ctx).ok_or(AlgorithmError::InsufficientLiquidity)?;

        let split_outputs: Vec<(usize, BigUint)> = full_outputs
            .iter()
            .filter(|(idx, _)| *idx < split_path_count)
            .cloned()
            .collect();
        let ranked: Vec<(usize, &Path<DepthAndPrice>)> = split_outputs
            .iter()
            .map(|(idx, _)| (*idx, &paths[*idx]))
            .collect();
        let disjoint = Self::select_disjoint(&ranked, self.max_paths);

        let disjoint_candidate =
            Self::build_disjoint_route(&paths, &disjoint, &ctx, order, self.num_chunks);
        let shared_candidate = Self::fill_and_spill(&paths, &full_outputs, &ctx, order);
        Ok(Self::choose_best_route(best_single, [disjoint_candidate, shared_candidate]))
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
    use std::sync::Arc;

    use alloy::primitives::U256;
    use num_traits::ToPrimitive;
    use tokio::sync::RwLock;
    use tycho_execution::encoding::models::Solution;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::{models::token::Token, Bytes},
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{addr, component, token_with_decimals},
        feed::market_data::{MarketData, MarketState},
        graph::GraphManager,
        types::{quote::OrderSide, BlockInfo, OrderQuote, QuoteStatus},
        MostLiquidAlgorithm,
    };

    fn weth_usdc_pool(weth_reserve: u128, usdc_reserve: u128) -> UniswapV2State {
        v2_pool(weth_reserve, 18, usdc_reserve, 6)
    }

    fn v2_pool(
        reserve_a: u128,
        decimals_a: u64,
        reserve_b: u128,
        decimals_b: u64,
    ) -> UniswapV2State {
        UniswapV2State::new(
            U256::from(reserve_a) * U256::from(10u64).pow(U256::from(decimals_a)),
            U256::from(reserve_b) * U256::from(10u64).pow(U256::from(decimals_b)),
        )
    }

    fn setup_weighted_market(
        pools: Vec<(&str, Token, Token, Box<dyn ProtocolSim>)>,
    ) -> (MarketData, PetgraphStableDiGraphManager<DepthAndPrice>) {
        let mut market = MarketState::new();
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });
        market.update_last_updated(BlockInfo::new(1, "0x01".to_string(), 0));

        let mut weights = Vec::new();
        for (pool_id, token_a, token_b, state) in pools {
            let weight_ab = edge_weight(state.as_ref(), &token_a, &token_b);
            let weight_ba = edge_weight(state.as_ref(), &token_b, &token_a);
            let tokens = vec![token_a.clone(), token_b.clone()];
            market.upsert_components(std::iter::once(component(pool_id, &tokens)));
            market.upsert_tokens(tokens);
            market.update_states([(pool_id.to_string(), state)]);
            weights.push((
                pool_id.to_string(),
                token_a.address,
                token_b.address,
                weight_ab,
                weight_ba,
            ));
        }

        let mut graph_manager = PetgraphStableDiGraphManager::<DepthAndPrice>::default();
        graph_manager.initialize_graph(&market.component_topology());
        for (pool_id, token_a, token_b, weight_ab, weight_ba) in weights {
            graph_manager
                .set_edge_weight(&pool_id, &token_a, &token_b, weight_ab, false)
                .expect("forward weight is set");
            graph_manager
                .set_edge_weight(&pool_id, &token_b, &token_a, weight_ba, false)
                .expect("reverse weight is set");
        }

        (MarketData::new(Arc::new(RwLock::new(market))), graph_manager)
    }

    fn edge_weight(state: &dyn ProtocolSim, token_in: &Token, token_out: &Token) -> DepthAndPrice {
        let spot_price = state
            .spot_price(token_in, token_out)
            .expect("spot price exists");
        let depth = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("limits exist")
            .0
            .to_f64()
            .expect("depth fits f64");
        DepthAndPrice { spot_price, depth }
    }

    async fn solve_split_route(
        market: MarketData,
        graph_manager: &PetgraphStableDiGraphManager<DepthAndPrice>,
        order: &Order,
        config: AlgorithmConfig,
    ) -> RouteResult {
        let algo = SplitAlgorithm::with_config(config).unwrap();
        algo.find_best_route(graph_manager.graph(), market, None, None, order)
            .await
            .expect("split route solves")
    }

    /// Two equally-deep WETH/USDC pools: a large order should split ~50/50 and beat any single
    /// path.
    #[tokio::test]
    async fn split_beats_single_path_on_two_equal_pools() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (market, graph_manager) = setup_weighted_market(vec![
            (
                "pool_a",
                weth.clone(),
                usdc.clone(),
                Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_b",
                weth.clone(),
                usdc.clone(),
                Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
            ),
        ]);

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

        let split = SplitAlgorithm::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market.clone(), None, None, &order)
            .await
            .expect("split solves");
        let ml = MostLiquidAlgorithm::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market, None, None, &order)
            .await
            .expect("ml solves");

        let split_paths = split
            .route()
            .swaps()
            .iter()
            .filter(|swap| swap.token_in() == &weth.address)
            .count();
        assert_eq!(split_paths, 2, "large order should use both pools");
        assert!(
            split.net_amount_out() > ml.net_amount_out(),
            "split ({}) should beat single-path ({})",
            split.net_amount_out(),
            ml.net_amount_out()
        );

        // Splitting 50/50 across two identical pools should be ~20% better than one pool here.
        let gain = split.net_amount_out().to_f64().unwrap() / ml.net_amount_out().to_f64().unwrap();
        assert!(gain > 1.15, "expected >15% gain from splitting, got {:.3}x", gain);
    }

    /// Two candidate paths share the same first pool, then diverge across shallow downstream pools.
    /// Pool-disjoint splitting cannot use both paths, but shared-pool fill-and-spill can.
    #[tokio::test]
    async fn split_uses_shared_prefix_when_downstream_liquidity_splits() {
        let src = token_with_decimals(0x01, "SRC", 18);
        let bridge = token_with_decimals(0x02, "BRG", 18);
        let dst = token_with_decimals(0x03, "DST", 18);
        let (market, graph_manager) = setup_weighted_market(vec![
            (
                "src_bridge",
                src.clone(),
                bridge.clone(),
                Box::new(v2_pool(10_000, 18, 10_000, 18)) as Box<dyn ProtocolSim>,
            ),
            (
                "bridge_dst_a",
                bridge.clone(),
                dst.clone(),
                Box::new(v2_pool(500, 18, 500, 18)) as Box<dyn ProtocolSim>,
            ),
            (
                "bridge_dst_b",
                bridge.clone(),
                dst.clone(),
                Box::new(v2_pool(500, 18, 500, 18)) as Box<dyn ProtocolSim>,
            ),
        ]);

        let order = Order::new(
            src.address.clone(),
            dst.address.clone(),
            BigUint::from(200u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();

        let split = SplitAlgorithm::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market.clone(), None, None, &order)
            .await
            .expect("split solves");
        let ml = MostLiquidAlgorithm::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market.clone(), None, None, &order)
            .await
            .expect("ml solves");

        assert!(
            split.net_amount_out() > ml.net_amount_out(),
            "shared-prefix split ({}) should beat single-path ({})",
            split.net_amount_out(),
            ml.net_amount_out()
        );

        let route_result = solve_split_route(market, &graph_manager, &order, config).await;
        let route = route_result.route();
        let component_ids: Vec<&str> = route
            .swaps()
            .iter()
            .map(|swap| swap.component_id())
            .collect();
        assert_eq!(
            component_ids
                .iter()
                .filter(|&&id| id == "src_bridge")
                .count(),
            1,
            "shared prefix should be merged into one executable swap"
        );
        assert!(component_ids.contains(&"bridge_dst_a"));
        assert!(component_ids.contains(&"bridge_dst_b"));

        let downstream_splits: Vec<f64> = route
            .swaps()
            .iter()
            .filter(|swap| swap.token_in() == &bridge.address)
            .map(|swap| *swap.split())
            .collect();
        assert_eq!(downstream_splits.len(), 2, "BRG should split downstream");
        assert!(
            downstream_splits
                .iter()
                .any(|split| *split > 0.0 && *split < 1.0),
            "one downstream branch should carry an explicit split"
        );
        assert!(
            downstream_splits.contains(&0.0),
            "one downstream branch should use the remainder convention"
        );
        for token in [&src.address, &bridge.address, &dst.address] {
            assert!(route.tokens().contains_key(token), "route token map should contain {token}");
        }

        let amount_out = route
            .swaps()
            .iter()
            .filter(|swap| swap.token_out() == order.token_out())
            .fold(BigUint::zero(), |acc, swap| acc + swap.amount_out());
        let quote = OrderQuote::new(
            "shared-prefix".to_string(),
            QuoteStatus::Success,
            order.amount().clone(),
            amount_out.clone(),
            route.total_gas(),
            amount_out,
            BlockInfo::new(1, "0x01".to_string(), 0),
            "split".to_string(),
            Bytes::from(order.sender().as_ref()),
            Bytes::from(order.effective_receiver().as_ref()),
            "1".to_string(),
        )
        .with_route(route.clone())
        .with_gas_price(route_result.gas_price().clone());
        Solution::try_from(&quote).expect("hardened split route should encode");
    }

    /// A tiny order shouldn't split (gas/impact make one pool optimal) and must not lose to
    /// single-path.
    #[tokio::test]
    async fn small_order_does_not_lose_to_single_path() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (market, graph_manager) = setup_weighted_market(vec![
            (
                "pool_a",
                weth.clone(),
                usdc.clone(),
                Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_b",
                weth.clone(),
                usdc.clone(),
                Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
            ),
        ]);

        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(10u64).pow(15), // 0.001 WETH
            OrderSide::Sell,
            addr(0xFF),
        );
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();

        let split = SplitAlgorithm::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market.clone(), None, None, &order)
            .await
            .expect("split solves");
        let ml = MostLiquidAlgorithm::with_config(config)
            .unwrap()
            .find_best_route(graph_manager.graph(), market, None, None, &order)
            .await
            .expect("ml solves");

        assert!(
            split.net_amount_out() >= ml.net_amount_out(),
            "split must never lose to single-path: split={} ml={}",
            split.net_amount_out(),
            ml.net_amount_out()
        );
    }
}
