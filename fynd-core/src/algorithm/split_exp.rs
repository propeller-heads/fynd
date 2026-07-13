//! Experimental split-routing variants for the routing-quality benchmark.
//!
//! These sharpen the incumbent [`SplitAlgorithm`](super::split::SplitAlgorithm) on its two
//! documented weaknesses: coarse allocation and the pool-disjoint restriction. All allocation runs
//! use an incremental water-fill: because constant-product / tick AMMs are path-independent in
//! cumulative input (one swap of `x` equals two sequential swaps summing to `x`), probing the
//! marginal of the *next* chunk against a committed overlay is identical to re-simulating at the
//! cumulative amount, but costs O(chunks) instead of O(chunks^2). The saved work funds a finer
//! grid.
//!
//! Naive fine-graining regresses on large trades: a smaller first chunk can fail the per-chunk gas
//! activation gate, so a profitable second path never turns on. The fix is two-phase — decide the
//! active path *set* at coarse granularity (correct activation), then refine the allocation over
//! that fixed set with no gate. Three strategies are exposed:
//!
//! * [`SplitStrategy::RefinedDisjoint`] — pool-disjoint, coarse-set + fine allocation.
//! * [`SplitStrategy::FillAndSpill`] — shared-pool overlay, coarse-set + fine allocation. Retained
//!   for research; on this market top paths are almost always pool-disjoint so it does not help.
//! * [`SplitStrategy::Portfolio`] — the recommended `split_max`. Returns the best net of the single
//!   path, an incumbent-equivalent coarse disjoint split (a floor with the same cost as the
//!   incumbent, so a tight timeout cannot starve it), and the finer refined disjoint split. It
//!   never loses to the incumbent while capturing the finer-allocation upside. Fill-and-spill and a
//!   wider path cap were benchmarked and left out: neither beat the refined disjoint split
//!   net-of-gas here.

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
    most_liquid::DepthAndPrice,
    split_discovery::{find_candidate_paths, CandidateSearchConfig},
    split_primitives::{build_split_route, HopDescriptor, PathAllocation, SimulatedHop},
    Algorithm, AlgorithmConfig, MostLiquidAlgorithm, NoPathReason,
};
use crate::{
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, Path, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult},
    AlgorithmError,
};

/// Maximum candidate paths simulated per order after heuristic ranking.
const DEFAULT_MAX_CANDIDATES: usize = 5000;
/// Cap on candidates from the bounded amount-aware discovery unioned into the candidate set
/// (matches the bounded discovery's own candidate cap in [`super::split_discovery`]).
const BOUNDED_DISCOVERY_CANDIDATES: usize = 128;
/// Maximum number of parallel paths in a split (matches the incumbent).
const DEFAULT_MAX_PATHS: usize = 4;
/// Chunk grid for the coarse set-selection pass (matches the incumbent's granularity).
const COARSE_CHUNKS: usize = 20;
/// Chunk grid for the fine allocation pass over the fixed active set.
const FINE_CHUNKS: usize = 256;
/// Number of top full-amount paths always considered for shared-pool fill-and-spill.
const SHARED_FULL_PATHS: usize = 8;
/// Number of full-amount-ranked paths probed with the first chunk for fill-and-spill.
const SHARED_MARGIN_PROBE_PATHS: usize = 32;
/// Number of marginal-probe winners added to the fill-and-spill candidate set.
const SHARED_MARGIN_PATHS: usize = 8;
/// Upper bound on fill-and-spill candidate paths.
const SHARED_MAX_CANDIDATES: usize = 12;

/// Which experimental allocation strategy an [`ExpSplitAlgorithm`] runs.
#[derive(Clone, Copy, Debug)]
pub enum SplitStrategy {
    /// Pool-disjoint paths, coarse set-selection then fine allocation.
    RefinedDisjoint,
    /// Shared-pool fill-and-spill, coarse set-selection then fine allocation.
    FillAndSpill,
    /// Best net of single-path, the incumbent-style coarse-disjoint floor, and refined-disjoint.
    Portfolio,
}

impl SplitStrategy {
    fn name(self) -> &'static str {
        match self {
            SplitStrategy::RefinedDisjoint => "split_incr",
            SplitStrategy::FillAndSpill => "split_ff",
            SplitStrategy::Portfolio => "split_max",
        }
    }
}

/// A fully-built split candidate: the assembled route plus its summed gross output and gas.
struct SplitCandidate {
    route: Route,
    gross: BigUint,
    gas: BigUint,
}

impl SplitCandidate {
    /// Net output in output-token terms (gross minus gas cost).
    fn net(
        &self,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        token_out: &Address,
    ) -> BigInt {
        let cost =
            ExpSplitAlgorithm::gas_cost_in_token(&self.gas, gas_price, token_prices, token_out);
        match cost {
            Some(c) => BigInt::from(self.gross.clone()) - BigInt::from(c),
            None => BigInt::from(self.gross.clone()),
        }
    }
}

/// Experimental split router.
pub struct ExpSplitAlgorithm {
    strategy: SplitStrategy,
    min_hops: usize,
    max_hops: usize,
    timeout: Duration,
    max_candidates: usize,
    max_paths: usize,
    connector_tokens: Option<HashSet<Address>>,
}

/// One simulated traversal of a path, with the resulting per-pool states so they can be committed.
struct StepResult {
    amount_out: BigUint,
    gas: BigUint,
    new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)>,
}

impl ExpSplitAlgorithm {
    /// Builds a refined-disjoint variant.
    ///
    /// Entry point for the `split_incr` research strategy exercised by the offline
    /// routing-quality benchmark, which is not part of this branch. The production portfolio
    /// reaches the same allocator internally via [`Self::disjoint_alloc`].
    #[allow(dead_code)]
    pub(crate) fn refined_disjoint(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Self::with_config(SplitStrategy::RefinedDisjoint, config)
    }

    /// Builds a fill-and-spill variant.
    ///
    /// Entry point for the `split_ff` research strategy exercised by the offline
    /// routing-quality benchmark, which is not part of this branch. The production portfolio
    /// reaches the same allocator internally via [`Self::fillspill_alloc`].
    #[allow(dead_code)]
    pub(crate) fn fill_and_spill(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Self::with_config(SplitStrategy::FillAndSpill, config)
    }

    /// Builds the portfolio variant (recommended).
    pub(crate) fn portfolio(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Self::with_config(SplitStrategy::Portfolio, config)
    }

    fn with_config(
        strategy: SplitStrategy,
        config: AlgorithmConfig,
    ) -> Result<Self, AlgorithmError> {
        Ok(Self {
            strategy,
            min_hops: config.min_hops(),
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            max_candidates: config
                .max_routes()
                .unwrap_or(DEFAULT_MAX_CANDIDATES)
                .max(DEFAULT_MAX_PATHS),
            max_paths: DEFAULT_MAX_PATHS,
            connector_tokens: config.connector_tokens().cloned(),
        })
    }

    /// Simulates `amount` through `path`, reading each pool from `overlay` if present else the base
    /// state. Returns the output, summed gas, and the resulting per-pool states so the caller can
    /// commit them into an overlay.
    fn simulate_step(
        path: &Path<DepthAndPrice>,
        market: &MarketState,
        overlay: &HashMap<ComponentId, Box<dyn ProtocolSim>>,
        amount: BigUint,
    ) -> Option<StepResult> {
        let mut current = amount;
        let mut total_gas = BigUint::zero();
        // Intra-path pool reuse: a pool touched twice in one path must see its own first swap.
        let mut local: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)> =
            Vec::with_capacity(path.len());

        for (address_in, edge, address_out) in path.iter() {
            let token_in = market.get_token(address_in)?;
            let token_out = market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let state = local
                .get(component_id)
                .map(Box::as_ref)
                .or_else(|| {
                    overlay
                        .get(component_id)
                        .map(Box::as_ref)
                })
                .or_else(|| market.get_simulation_state(component_id))?;
            let result = state
                .get_amount_out(current.clone(), token_in, token_out)
                .ok()?;
            total_gas += &result.gas;
            local.insert(component_id.clone(), result.new_state.clone_box());
            new_states.push((component_id.clone(), result.new_state));
            current = result.amount;
        }
        Some(StepResult { amount_out: current, gas: total_gas, new_states })
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

    /// Greedily selects pool-disjoint path indices from `ranked` (best first), up to `max_paths`.
    fn select_disjoint(ranked: &[Path<DepthAndPrice>], max_paths: usize) -> Vec<usize> {
        let mut used: HashSet<&ComponentId> = HashSet::new();
        let mut selected = Vec::new();
        for (idx, path) in ranked.iter().enumerate() {
            let components: Vec<&ComponentId> = path
                .edge_iter()
                .iter()
                .map(|e| &e.component_id)
                .collect();
            if components
                .iter()
                .any(|c| used.contains(*c))
            {
                continue;
            }
            for c in components {
                used.insert(c);
            }
            selected.push(idx);
            if selected.len() >= max_paths {
                break;
            }
        }
        selected
    }

    /// Shared setup: enumerate + rank candidates, simulate at full amount, pick best single path.
    #[allow(clippy::type_complexity)]
    async fn setup<'a>(
        &self,
        graph: &'a StableDiGraph<DepthAndPrice>,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
        start: Instant,
    ) -> Result<
        (Vec<Path<'a, DepthAndPrice>>, MarketState, BigUint, RouteResult, Option<TokenGasPrices>),
        AlgorithmError,
    > {
        let token_prices = if let Some(ref derived) = derived {
            derived
                .read()
                .await
                .token_prices()
                .cloned()
        } else {
            None
        };

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
        let mut paths: Vec<Path<DepthAndPrice>> = scored
            .into_iter()
            .map(|(p, _)| p)
            .collect();

        let timeout_ms = self.timeout.as_millis() as u64;
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
            // Bounded amount-aware discovery (see `super::split_discovery`): union its candidates
            // ahead of the pre-ranked set, so connector/anchor routes (incl. the native-ETH
            // sentinel) survive the spot×depth truncation. Discovery failure is not fatal — the
            // pre-ranked set already guarantees a route.
            let bounded = find_candidate_paths(
                graph,
                &view,
                order,
                CandidateSearchConfig {
                    min_hops: self.min_hops,
                    max_hops: self.max_hops,
                    max_candidates: BOUNDED_DISCOVERY_CANDIDATES,
                    connector_tokens: self.connector_tokens.as_ref(),
                    source_token: order.token_in(),
                    start: &start,
                    timeout_ms,
                },
            );
            if let Ok((bounded_paths, _)) = bounded {
                let mut keys: HashSet<Vec<ComponentId>> = paths.iter().map(path_key).collect();
                let mut union = Vec::with_capacity(bounded_paths.len() + paths.len());
                for path in bounded_paths {
                    if keys.insert(path_key(&path)) {
                        union.push(path);
                    }
                }
                union.append(&mut paths);
                paths = union;
            }
            let component_ids: HashSet<ComponentId> = paths
                .iter()
                .flat_map(|p| {
                    p.edge_iter()
                        .iter()
                        .map(|e| e.component_id.clone())
                })
                .collect();
            let subset = view.extract_subset_with_overlay(&component_ids);
            drop(view);
            subset
        };
        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();

        let amount_in = order.amount().clone();
        let mut best_single: Option<RouteResult> = None;
        let mut full_outputs: Vec<(usize, BigUint)> = Vec::new();

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

        full_outputs.sort_by(|(_, a), (_, b)| b.cmp(a));
        let ordered: Vec<Path<DepthAndPrice>> = full_outputs
            .into_iter()
            .map(|(idx, _)| paths[idx].clone())
            .collect();

        Ok((ordered, market, gas_price, best_single, token_prices))
    }
}

impl Algorithm for ExpSplitAlgorithm {
    type GraphType = StableDiGraph<DepthAndPrice>;
    type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        self.strategy.name()
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

        let (ordered, market, gas_price, best_single, token_prices) = self
            .setup(graph, market, label, derived, order, start)
            .await?;
        let tp = token_prices.as_ref();
        let token_out = order.token_out();

        // Collect the split candidates the strategy considers.
        let mut candidates: Vec<SplitCandidate> = Vec::new();
        match self.strategy {
            SplitStrategy::RefinedDisjoint => {
                if let Some(c) = self.disjoint_alloc(
                    &ordered,
                    &market,
                    &gas_price,
                    tp,
                    order,
                    start,
                    FINE_CHUNKS,
                    self.max_paths,
                ) {
                    candidates.push(c);
                }
            }
            SplitStrategy::FillAndSpill => {
                if let Some(c) = self.fillspill_alloc(
                    &ordered,
                    &market,
                    &gas_price,
                    tp,
                    order,
                    start,
                    FINE_CHUNKS,
                ) {
                    candidates.push(c);
                }
            }
            SplitStrategy::Portfolio => {
                // Incumbent-equivalent floor first: single gated coarse pass, same cost as the
                // incumbent, so a tight timeout cannot starve it into a single-path fallback while
                // the incumbent still splits.
                if let Some(c) = self.floor_alloc(
                    &ordered,
                    &market,
                    &gas_price,
                    tp,
                    order,
                    start,
                    self.max_paths,
                ) {
                    candidates.push(c);
                }
                // Refined fine allocation over the same active set (bonus; may be starved).
                if let Some(c) = self.disjoint_alloc(
                    &ordered,
                    &market,
                    &gas_price,
                    tp,
                    order,
                    start,
                    FINE_CHUNKS,
                    self.max_paths,
                ) {
                    candidates.push(c);
                }
                // Shared-pool fill-and-spill with marginal-probe candidate selection. It captures
                // wins that are tree routes splitting at an intermediate token (paths sharing a
                // pool), which no pool-disjoint allocation can express.
                if let Some(c) = self.fillspill_alloc(
                    &ordered,
                    &market,
                    &gas_price,
                    tp,
                    order,
                    start,
                    FINE_CHUNKS,
                ) {
                    candidates.push(c);
                }
            }
        }

        // Return the best candidate if it beats the single path, else the single path.
        let mut best_net = best_single.net_amount_out().clone();
        let mut best_candidate: Option<SplitCandidate> = None;
        for cand in candidates {
            let net = cand.net(&gas_price, tp, token_out);
            if net > best_net {
                best_net = net;
                best_candidate = Some(cand);
            }
        }
        match best_candidate {
            Some(cand) => Ok(RouteResult::new(cand.route, best_net, gas_price)),
            None => Ok(best_single),
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

impl ExpSplitAlgorithm {
    /// Incumbent-equivalent floor split: a single gated coarse water-fill over the disjoint set,
    /// same chunk grid and cost as [`SplitAlgorithm`](super::split::SplitAlgorithm). Because it
    /// does exactly the incumbent's allocation work on the shared clock, it cannot be starved
    /// by a tighter timeout when the incumbent would still produce a split — this is what makes
    /// the portfolio's never-lose guarantee hold under time pressure, not just in the untimed
    /// case.
    #[allow(clippy::too_many_arguments)]
    fn floor_alloc(
        &self,
        ordered: &[Path<DepthAndPrice>],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
        max_paths: usize,
    ) -> Option<SplitCandidate> {
        let disjoint = Self::select_disjoint(ordered, max_paths);
        if disjoint.len() < 2 {
            return None;
        }
        let alloc = self.disjoint_waterfill(
            ordered,
            &disjoint,
            market,
            gas_price,
            token_prices,
            order,
            start,
            COARSE_CHUNKS,
            true,
        )?;
        self.build_disjoint_legs(ordered, &disjoint, &alloc, market, token_prices, order)
    }

    /// Coarse set-selection then fine allocation over pool-disjoint paths. Refines the floor split
    /// on a finer grid; if a tight timeout starves either pass this returns `None` and the caller
    /// falls back to the floor candidate.
    #[allow(clippy::too_many_arguments)]
    fn disjoint_alloc(
        &self,
        ordered: &[Path<DepthAndPrice>],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
        fine_chunks: usize,
        max_paths: usize,
    ) -> Option<SplitCandidate> {
        let disjoint = Self::select_disjoint(ordered, max_paths);
        if disjoint.len() < 2 {
            return None;
        }

        // Phase 1: coarse water-fill with the gas-activation gate to pick the active set.
        let coarse = self.disjoint_waterfill(
            ordered,
            &disjoint,
            market,
            gas_price,
            token_prices,
            order,
            start,
            COARSE_CHUNKS,
            true,
        )?;
        let active: Vec<usize> = disjoint
            .iter()
            .copied()
            .zip(coarse.iter())
            .filter(|(_, amt)| !amt.is_zero())
            .map(|(idx, _)| idx)
            .collect();
        if active.is_empty() {
            return None;
        }

        // Phase 2: fine water-fill over the fixed active set, no gate (gas already justified).
        let chunks = fine_chunks.max(COARSE_CHUNKS);
        let fine = self.disjoint_waterfill(
            ordered,
            &active,
            market,
            gas_price,
            token_prices,
            order,
            start,
            chunks,
            false,
        )?;
        self.build_disjoint_legs(ordered, &active, &fine, market, token_prices, order)
    }

    /// Simulates `amount` through `path`, reading and committing pool states via `overrides`, and
    /// returns the allocation consumed by [`build_split_route`].
    fn allocation_commit(
        path: &Path<DepthAndPrice>,
        market: &MarketState,
        overrides: &mut HashMap<ComponentId, Box<dyn ProtocolSim>>,
        amount: BigUint,
        flow_fraction: f64,
    ) -> Option<PathAllocation> {
        let amount_in = amount;
        let mut current = amount_in.clone();
        let mut hops = Vec::with_capacity(path.len());

        for (address_in, edge, address_out) in path.iter() {
            let token_in = market.get_token(address_in)?;
            let token_out = market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let state = overrides
                .get(component_id)
                .map(Box::as_ref)
                .or_else(|| market.get_simulation_state(component_id))?;
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

    /// Assembles a route from the allocations via the shared split primitives (topological swap
    /// order, tycho-execution remainder-split convention, route token map) and derives the
    /// candidate's gross output and gas from the assembled route.
    fn candidate_from_allocations(
        allocations: &[PathAllocation],
        market: &MarketState,
        order: &Order,
    ) -> Option<SplitCandidate> {
        let route = build_split_route(allocations, market, order).ok()?;
        let token_out = order.token_out();
        let gross = route
            .swaps()
            .iter()
            .filter(|s| s.token_out() == token_out)
            .fold(BigUint::zero(), |acc, s| acc + s.amount_out());
        if gross.is_zero() {
            return None;
        }
        let gas = route.total_gas();
        Some(SplitCandidate { route, gross, gas })
    }

    /// Builds one independent leg per path at its allocated amount. `subset` and `alloc` are
    /// aligned by index; pool-disjoint paths never interfere, so each leg is a real independent
    /// simulation.
    fn build_disjoint_legs(
        &self,
        ordered: &[Path<DepthAndPrice>],
        subset: &[usize],
        alloc: &[BigUint],
        market: &MarketState,
        _token_prices: Option<&TokenGasPrices>,
        order: &Order,
    ) -> Option<SplitCandidate> {
        let amount_in = order.amount().clone();
        let mut allocations = Vec::new();
        for (i, &path_idx) in subset.iter().enumerate() {
            if alloc[i].is_zero() {
                continue;
            }
            // Fresh overrides per leg: legs are pool-disjoint, but a pool reused within one path
            // must still see its own first swap.
            let mut overrides: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
            let allocation = Self::allocation_commit(
                &ordered[path_idx],
                market,
                &mut overrides,
                alloc[i].clone(),
                ratio(&alloc[i], &amount_in),
            )?;
            allocations.push(allocation);
        }
        if allocations.is_empty() {
            return None;
        }
        Self::candidate_from_allocations(&allocations, market, order)
    }

    /// Incremental water-fill over a set of pool-disjoint paths. Returns the amount allocated to
    /// each path in `subset` order. With `gate`, a path only activates when its first chunk covers
    /// its gas; without it, every path is eligible (used once the active set is fixed).
    #[allow(clippy::too_many_arguments)]
    fn disjoint_waterfill(
        &self,
        ordered: &[Path<DepthAndPrice>],
        subset: &[usize],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
        num_chunks: usize,
        gate: bool,
    ) -> Option<Vec<BigUint>> {
        let amount_in = order.amount().clone();
        let num_chunks = num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            return None;
        }
        let remainder = &amount_in - &base_chunk * num_chunks;
        let timeout_ms = self.timeout.as_millis() as u64;
        let k = subset.len();

        let mut committed: Vec<HashMap<ComponentId, Box<dyn ProtocolSim>>> =
            (0..k).map(|_| HashMap::new()).collect();
        let mut cum_in: Vec<BigUint> = vec![BigUint::zero(); k];
        let mut activated: Vec<bool> = vec![!gate; k];

        for chunk_idx in 0..num_chunks {
            if start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };

            let mut best: Option<(usize, BigInt, StepResult)> = None;
            for (i, &path_idx) in subset.iter().enumerate() {
                let Some(step) =
                    Self::simulate_step(&ordered[path_idx], market, &committed[i], chunk.clone())
                else {
                    continue;
                };
                let gross_marginal = BigInt::from(step.amount_out.clone());
                let net_marginal = if activated[i] {
                    gross_marginal
                } else {
                    let activation = Self::gas_cost_in_token(
                        &step.gas,
                        gas_price,
                        token_prices,
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
                    best = Some((i, net_marginal, step));
                }
            }

            let Some((best_i, _, step)) = best else {
                break;
            };
            for (id, state) in step.new_states {
                committed[best_i].insert(id, state);
            }
            cum_in[best_i] += &chunk;
            activated[best_i] = true;
        }
        Some(cum_in)
    }

    /// Selects fill-and-spill candidates: the top full-amount paths plus the best first-chunk
    /// marginal probes. The probe is what makes intermediate-token splits (tree routes) reachable:
    /// the extra path often ranks poorly at full size but wins on the margin.
    fn select_shared_candidates(
        &self,
        ordered: &[Path<DepthAndPrice>],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
    ) -> Vec<usize> {
        let mut candidates: Vec<usize> = (0..ordered.len().min(SHARED_FULL_PATHS)).collect();
        let first_chunk = order.amount() / COARSE_CHUNKS;
        if first_chunk.is_zero() {
            return candidates;
        }
        let timeout_ms = self.timeout.as_millis() as u64;
        let empty: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut marginal: Vec<(usize, BigInt)> = Vec::new();
        for (idx, path) in ordered
            .iter()
            .enumerate()
            .take(SHARED_MARGIN_PROBE_PATHS)
        {
            if start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let Some(step) = Self::simulate_step(path, market, &empty, first_chunk.clone()) else {
                continue;
            };
            let activation =
                Self::gas_cost_in_token(&step.gas, gas_price, token_prices, order.token_out())
                    .map(BigInt::from)
                    .unwrap_or_else(BigInt::zero);
            marginal.push((idx, BigInt::from(step.amount_out) - activation));
        }
        marginal.sort_by(|(_, a), (_, b)| b.cmp(a));
        for (idx, net) in marginal
            .into_iter()
            .take(SHARED_MARGIN_PATHS)
        {
            if net <= BigInt::zero() {
                continue;
            }
            if !candidates.contains(&idx) {
                candidates.push(idx);
            }
            if candidates.len() >= SHARED_MAX_CANDIDATES {
                break;
            }
        }
        candidates
    }

    /// Coarse set-selection then fine allocation with shared-pool fill-and-spill.
    #[allow(clippy::too_many_arguments)]
    fn fillspill_alloc(
        &self,
        ordered: &[Path<DepthAndPrice>],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
        fine_chunks: usize,
    ) -> Option<SplitCandidate> {
        let cand =
            self.select_shared_candidates(ordered, market, gas_price, token_prices, order, start);
        if cand.len() < 2 {
            return None;
        }

        // Phase 1: coarse gated pass to choose the active candidate set.
        let (coarse_counts, _) = self.fillspill_waterfill(
            ordered,
            &cand,
            market,
            gas_price,
            token_prices,
            order,
            start,
            COARSE_CHUNKS,
            true,
        )?;
        let active: Vec<usize> = cand
            .iter()
            .copied()
            .zip(coarse_counts.iter())
            .filter(|(_, count)| **count > 0)
            .map(|(idx, _)| idx)
            .collect();
        if active.len() < 2 {
            return None;
        }

        // Phase 2: fine ungated pass over the active set, with the commit schedule for replay.
        let chunks = fine_chunks.max(COARSE_CHUNKS);
        let (_, schedule) = self.fillspill_waterfill(
            ordered,
            &active,
            market,
            gas_price,
            token_prices,
            order,
            start,
            chunks,
            false,
        )?;
        if schedule.is_empty() {
            return None;
        }

        self.build_fillspill_route(ordered, &active, market, &schedule, order)
    }

    /// Incremental fill-and-spill water-fill over a single shared overlay. Returns the chunk count
    /// each candidate received and the ordered commit schedule of `(active_index, chunk_amount)`.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn fillspill_waterfill(
        &self,
        ordered: &[Path<DepthAndPrice>],
        subset: &[usize],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
        num_chunks: usize,
        gate: bool,
    ) -> Option<(Vec<usize>, Vec<(usize, BigUint)>)> {
        let amount_in = order.amount().clone();
        let num_chunks = num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            return None;
        }
        let remainder = &amount_in - &base_chunk * num_chunks;
        let timeout_ms = self.timeout.as_millis() as u64;

        let mut overlay: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut activated: Vec<bool> = vec![!gate; subset.len()];
        let mut active_count = if gate { 0 } else { subset.len() };
        let mut counts: Vec<usize> = vec![0; subset.len()];
        let mut schedule: Vec<(usize, BigUint)> = Vec::with_capacity(num_chunks);

        for chunk_idx in 0..num_chunks {
            if start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };

            let mut best: Option<(usize, BigInt, StepResult)> = None;
            for (i, &path_idx) in subset.iter().enumerate() {
                if !activated[i] && active_count >= self.max_paths {
                    continue;
                }
                let Some(step) =
                    Self::simulate_step(&ordered[path_idx], market, &overlay, chunk.clone())
                else {
                    continue;
                };
                let gross_marginal = BigInt::from(step.amount_out.clone());
                let net_marginal = if activated[i] {
                    gross_marginal
                } else {
                    let activation = Self::gas_cost_in_token(
                        &step.gas,
                        gas_price,
                        token_prices,
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
                    best = Some((i, net_marginal, step));
                }
            }

            let Some((best_i, _, step)) = best else {
                break;
            };
            for (id, state) in step.new_states {
                overlay.insert(id, state);
            }
            if !activated[best_i] {
                activated[best_i] = true;
                active_count += 1;
            }
            counts[best_i] += 1;
            schedule.push((best_i, chunk));
        }
        Some((counts, schedule))
    }

    /// Rebuilds the fill-and-spill result as one leg per active path at its total allocated
    /// amount, committed sequentially (largest allocation first) against a shared overlay — the
    /// same execution model the router applies on-chain.
    fn build_fillspill_route(
        &self,
        ordered: &[Path<DepthAndPrice>],
        active: &[usize],
        market: &MarketState,
        schedule: &[(usize, BigUint)],
        order: &Order,
    ) -> Option<SplitCandidate> {
        let amount_in = order.amount().clone();
        let mut cand_in: Vec<BigUint> = vec![BigUint::zero(); active.len()];
        for (i, chunk) in schedule {
            cand_in[*i] += chunk;
        }
        let mut execution_order: Vec<usize> = (0..active.len())
            .filter(|&i| !cand_in[i].is_zero())
            .collect();
        if execution_order.len() < 2 {
            return None;
        }
        execution_order.sort_by(|&a, &b| cand_in[b].cmp(&cand_in[a]));

        let mut overrides: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut allocations = Vec::new();
        for i in execution_order {
            let allocation = Self::allocation_commit(
                &ordered[active[i]],
                market,
                &mut overrides,
                cand_in[i].clone(),
                ratio(&cand_in[i], &amount_in),
            )?;
            allocations.push(allocation);
        }
        Self::candidate_from_allocations(&allocations, market, order)
    }
}

/// A path's identity for dedup: its ordered component-id sequence.
fn path_key(path: &Path<DepthAndPrice>) -> Vec<ComponentId> {
    path.edge_iter()
        .iter()
        .map(|e| e.component_id.clone())
        .collect()
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
    use std::time::Duration;

    use num_bigint::BigUint;
    use num_traits::ToPrimitive;

    use super::*;
    use crate::{
        algorithm::{
            split_test_harness::{
                optimal_two_pool_output, split_metrics, two_equal_weth_usdc,
                TWO_EQUAL_USDC_RESERVE, TWO_EQUAL_WETH_RESERVE,
            },
            test_utils::addr,
        },
        graph::GraphManager,
        types::quote::OrderSide,
    };

    fn config() -> AlgorithmConfig {
        config_ms(2000)
    }

    fn config_ms(ms: u64) -> AlgorithmConfig {
        AlgorithmConfig::new(1, 3, Duration::from_millis(ms), None).unwrap()
    }

    fn whole_weth_order(token_in: &Address, token_out: &Address, weth: u64) -> Order {
        Order::new(
            token_in.clone(),
            token_out.clone(),
            BigUint::from(weth) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        )
    }

    /// The portfolio splits a large order across both equal pools.
    #[tokio::test]
    async fn portfolio_splits_two_equal_pools() {
        let m = two_equal_weth_usdc(1);
        let order = whole_weth_order(&m.weth, &m.usdc, 500);

        let result = ExpSplitAlgorithm::portfolio(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("portfolio solves");
        let (_, path_count, _) = split_metrics(&result, &m.weth, &m.usdc);
        assert_eq!(path_count, 2, "large order should use both pools");
    }

    /// A tiny order must never lose to the single path.
    #[tokio::test]
    async fn portfolio_small_order_no_loss() {
        let m = two_equal_weth_usdc(1);
        let order = Order::new(
            m.weth.clone(),
            m.usdc.clone(),
            BigUint::from(10u64).pow(15),
            OrderSide::Sell,
            addr(0xFF),
        );

        let portfolio = ExpSplitAlgorithm::portfolio(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("portfolio solves");
        let single = MostLiquidAlgorithm::with_config(config())
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

        let (portfolio_net, _, _) = split_metrics(&portfolio, &m.weth, &m.usdc);
        let (single_net, _, _) = split_metrics(&single, &m.weth, &m.usdc);
        assert!(
            portfolio_net >= single_net,
            "portfolio must never lose to single-path: portfolio={portfolio_net} ml={single_net}",
        );
    }

    /// On two equal fee-free pools the portfolio's gross output must come within a tight tolerance
    /// of the analytical two-pool optimum (a 50/50 split), confirming the fine 256-chunk allocation
    /// finds the optimal allocation, not just any splitting one.
    #[tokio::test]
    async fn portfolio_output_near_two_pool_optimum() {
        let m = two_equal_weth_usdc(1);
        let trade = 500u64;
        let order = whole_weth_order(&m.weth, &m.usdc, trade);

        let portfolio = ExpSplitAlgorithm::portfolio(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("portfolio solves");
        let (_, path_count, gross) = split_metrics(&portfolio, &m.weth, &m.usdc);
        assert_eq!(path_count, 2, "the optimum uses both pools");

        // Fee-free reserves in raw units, so the no-fee two-pool optimum applies directly.
        let reserve_in = TWO_EQUAL_WETH_RESERVE as f64 * 1e18;
        let reserve_out = TWO_EQUAL_USDC_RESERVE as f64 * 1e6;
        let trade_amount = trade as f64 * 1e18;
        let (_, optimum) =
            optimal_two_pool_output(reserve_in, reserve_out, reserve_in, reserve_out, trade_amount);

        let gross = gross.to_f64().unwrap();
        assert!(
            gross >= optimum * 0.999 && gross <= optimum * 1.0001,
            "portfolio gross {gross} should be within 0.1% of the two-pool optimum {optimum}",
        );
    }

    /// Under a tight timeout the portfolio must still not lose to the best single path: the floor
    /// split does exactly the incumbent's coarse work, so a tight timeout cannot starve it into a
    /// single-path fallback while a split would still win.
    #[tokio::test]
    async fn portfolio_no_loss_under_tight_timeout() {
        for ms in [1u64, 5, 50] {
            let m = two_equal_weth_usdc(1_000_000_000);
            let order = whole_weth_order(&m.weth, &m.usdc, 500);

            let portfolio = ExpSplitAlgorithm::portfolio(config_ms(ms))
                .unwrap()
                .find_best_route(
                    m.weighted.graph(),
                    m.market.clone(),
                    None,
                    Some(m.derived.clone()),
                    &order,
                )
                .await
                .expect("portfolio solves");
            let (portfolio_net, _, _) = split_metrics(&portfolio, &m.weth, &m.usdc);

            let single = MostLiquidAlgorithm::with_config(config_ms(ms))
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
            let (single_net, _, _) = split_metrics(&single, &m.weth, &m.usdc);
            assert!(
                portfolio_net >= single_net,
                "portfolio lost to single-path under {ms}ms timeout: \
                 portfolio={portfolio_net} single={single_net}",
            );
        }
    }

    /// Gross output should scale with allocation: the fine grid must not overstate a leg's output.
    #[tokio::test]
    async fn portfolio_gross_is_positive_and_sane() {
        let m = two_equal_weth_usdc(1);
        let order = whole_weth_order(&m.weth, &m.usdc, 100);

        let result = ExpSplitAlgorithm::portfolio(config())
            .unwrap()
            .find_best_route(
                m.weighted.graph(),
                m.market.clone(),
                None,
                Some(m.derived.clone()),
                &order,
            )
            .await
            .expect("solves");
        let (_, _, gross) = split_metrics(&result, &m.weth, &m.usdc);
        assert!(gross.to_f64().unwrap() > 0.0);
    }
}
