//! Water-fill split-routing algorithm (`water_fill`).
//!
//! For large orders, price impact makes it better to split the order across several parallel routes
//! so the marginal price stays low. `WaterFillAlgorithm` builds up to four candidate routes and
//! returns the one with the best output net of gas, never worse than the best single path.
//!
//! The four candidates are the best single path plus three ways to split it:
//!
//! * **20-chunk disjoint split** — splits across paths that share no pool, in 20 chunks. This is
//!   the safety net: it is cheap and always finishes, so a tight timeout cannot cut off the split
//!   while leaving the single path. That is what makes the never-lose guarantee hold under time
//!   pressure.
//! * **256-chunk disjoint split** — the same pool-disjoint paths in 256 chunks, in two phases:
//!   first pick which paths to use at coarse (20-chunk) granularity, where each path's share is
//!   large enough for its gas-activation gate to be meaningful; then allocate across the chosen
//!   paths at fine (256-chunk) granularity with the gate off, since their gas is already covered.
//! * **Fill-and-spill** — a split that lets paths share a pool and branch at an intermediate token
//!   (a tree route), which the pool-disjoint splits cannot express.
//!
//! Every split fills the order in chunks: for each chunk, it checks how much each path returns for
//! that chunk and gives the chunk to the best one. Because constant-product / tick AMMs are
//! path-independent in cumulative input (one swap of `x` equals two back-to-back swaps summing to
//! `x`), it simulates only the next chunk against the pool state committed so far — O(chunks)
//! instead of O(chunks^2). That saved work makes the 256-chunk split affordable.
//!
//! Candidate discovery (the "Candidate discovery" section below) combines an exhaustive path
//! enumeration with a bounded, amount-aware frontier search, so connector and anchor routes
//! (including the native-ETH sentinel) survive the spot × depth cutoff. Every returned route is
//! assembled through the shared split primitives and can be encoded on-chain.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, instrument};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim, tycho_core::models::Address,
};

use super::{
    most_liquid::DepthAndPrice,
    sim_guard::GuardedProtocolSim,
    split_primitives::{build_split_route, HopDescriptor, PathAllocation, SimulatedHop},
    Algorithm, AlgorithmConfig, MostLiquidAlgorithm, NoPathReason,
};
use crate::{
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketDataView, MarketState, StateLabel},
    graph::{EdgeData, Path, PetgraphStableDiGraphManager, RoutingGraph},
    types::{ComponentId, Order, Route, RouteResult},
    AlgorithmError,
};

/// Maximum candidate paths simulated per order after heuristic ranking.
const DEFAULT_MAX_CANDIDATES: usize = 5000;
/// Cap on candidates from the bounded amount-aware discovery added to the candidate set
/// (matches the bounded discovery's own cap; see the discovery section below).
const BOUNDED_DISCOVERY_CANDIDATES: usize = 128;
/// Maximum number of parallel paths in a split.
const DEFAULT_MAX_PATHS: usize = 4;
/// Chunk grid for the coarse set-selection pass.
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
/// Candidate states retained per intermediate token during bounded discovery expansion.
const CANDIDATE_STATES_PER_NODE: usize = 4;
/// Candidate edge expansions from one path state during discovery.
const CANDIDATE_EDGES_PER_STATE: usize = 16;
/// Parallel pools kept for a discovery edge directly into the target token.
const CANDIDATE_DIRECT_EDGES_PER_TOKEN: usize = 4;
/// Parallel pools kept for a discovery edge into an anchor or configured connector token.
const CANDIDATE_CONNECTOR_EDGES_PER_TOKEN: usize = 2;
/// Number of highest-connectivity tokens taken as bounded-discovery anchors, derived per solve
/// from the graph (see `derive_anchor_tokens`).
const DERIVED_ANCHOR_COUNT: usize = 16;
/// Exchange-refinement step floor: the pass stops once `delta` falls below one fine chunk divided
/// by this factor, i.e. `amount_in / (fine_chunks * EXCHANGE_DELTA_FLOOR)`.
const EXCHANGE_DELTA_FLOOR: usize = 64;
/// Safety bound on trial simulations across the whole exchange-refinement pass.
const EXCHANGE_MAX_SIMS: usize = 400;

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
            WaterFillAlgorithm::gas_cost_in_token(&self.gas, gas_price, token_prices, token_out);
        match cost {
            Some(c) => BigInt::from(self.gross.clone()) - BigInt::from(c),
            None => BigInt::from(self.gross.clone()),
        }
    }
}

/// Splits an order across pool-disjoint (and, via fill-and-spill, pool-sharing) paths to reduce
/// price impact, returning the best net of the single path and several split allocations.
pub struct WaterFillAlgorithm {
    min_hops: usize,
    max_hops: usize,
    timeout: Duration,
    max_candidates: usize,
    max_paths: usize,
    connector_tokens: Option<HashSet<Address>>,
}

/// A candidate reallocation in the exchange-refinement pass: shift one `delta` of input from the
/// over-allocated `donor` to the under-allocated `recipient`, carrying the two paths' recomputed
/// net outputs and the resulting gain in summed net output.
struct ExchangeMove {
    donor: usize,
    recipient: usize,
    donor_net: BigInt,
    recip_net: BigInt,
    gain: BigInt,
}

/// One simulated traversal of a path, with the resulting per-pool states so they can be committed.
struct StepResult {
    amount_out: BigUint,
    gas: BigUint,
    new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)>,
}

/// Shared inputs threaded through every split-allocation pass: the ranked candidate paths, the
/// market snapshot, gas pricing, and the order under a single solve clock. Bundled so the
/// allocation methods take one context instead of the same six references each.
struct SplitContext<'a, 'g> {
    ordered: &'a [Path<'g, DepthAndPrice>],
    market: &'a MarketState,
    gas_price: &'a BigUint,
    token_prices: Option<&'a TokenGasPrices>,
    order: &'a Order,
    start: Instant,
}

/// Output of the shared setup pass: candidate paths ranked by full-amount output, the market
/// subset they touch, the effective gas price, the best single path (if one fills the order), and
/// token gas prices for gas-aware ranking.
struct SetupResult<'a> {
    ordered: Vec<Path<'a, DepthAndPrice>>,
    market: MarketState,
    gas_price: BigUint,
    best_single: Option<RouteResult>,
    token_prices: Option<TokenGasPrices>,
}

impl WaterFillAlgorithm {
    /// Creates a `WaterFillAlgorithm` from an `AlgorithmConfig`.
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
        // A pool touched twice in one path must see its own first swap, which needs an intra-path
        // overlay carried across hops. That reuse is rare, so only pay the per-hop state clone when
        // the path actually repeats a pool; the common case skips the clone entirely.
        let path_reuses_pool = {
            let mut seen: HashSet<&ComponentId> = HashSet::with_capacity(path.len());
            !path
                .edge_iter()
                .iter()
                .all(|e| seen.insert(&e.component_id))
        };
        let mut intra_path_states: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)> =
            Vec::with_capacity(path.len());

        for (address_in, edge, address_out) in path.iter() {
            let token_in = market.get_token(address_in)?;
            let token_out = market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let state = intra_path_states
                .get(component_id)
                .map(Box::as_ref)
                .or_else(|| {
                    overlay
                        .get(component_id)
                        .map(Box::as_ref)
                })
                .or_else(|| market.get_simulation_state(component_id))?;
            let result = state
                .get_amount_out_guarded(current.clone(), token_in, token_out)
                .ok()?;
            total_gas += &result.gas;
            if path_reuses_pool {
                intra_path_states.insert(component_id.clone(), result.new_state.clone_box());
            }
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

    /// A path's gas converted to output-token terms as a signed amount, or zero when no gas price
    /// is available. Subtracted from gross output to get net, so gas-blind solves fall back to
    /// gross ranking rather than erroring.
    fn activation_cost(ctx: &SplitContext, gas: &BigUint) -> BigInt {
        Self::gas_cost_in_token(gas, ctx.gas_price, ctx.token_prices, ctx.order.token_out())
            .map(BigInt::from)
            .unwrap_or_else(BigInt::zero)
    }

    /// Picks up to `max_paths` paths that share no pool, so their outputs can be summed without
    /// re-simulating — two paths through the same pool compete for its liquidity, so their separate
    /// outputs would not add up.
    ///
    /// Walks `ranked` best first and keeps a path only if none of its pools are already used by a
    /// kept path, skipping it otherwise. Returns the kept paths' indices into `ranked`.
    fn select_disjoint(ranked: &[Path<DepthAndPrice>], max_paths: usize) -> Vec<usize> {
        let mut visited_pools: HashSet<&ComponentId> = HashSet::new();
        let mut selected = Vec::new();
        for (idx, path) in ranked.iter().enumerate() {
            let path_pools: Vec<&ComponentId> = path
                .edge_iter()
                .iter()
                .map(|e| &e.component_id)
                .collect();
            if path_pools
                .iter()
                .any(|c| visited_pools.contains(*c))
            {
                continue;
            }
            for c in path_pools {
                visited_pools.insert(c);
            }
            selected.push(idx);
            if selected.len() >= max_paths {
                break;
            }
        }
        selected
    }

    /// Shared setup: enumerate + rank candidates, simulate at full amount, pick the best single
    /// path if any (a path that fails at the full amount is kept as a split-only candidate).
    #[instrument(level = "debug", skip_all)]
    async fn setup<'a>(
        &self,
        graph: &'a RoutingGraph<DepthAndPrice>,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
        start: Instant,
    ) -> Result<SetupResult<'a>, AlgorithmError> {
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
            // Bounded amount-aware discovery (see the discovery section below): union its
            // candidates ahead of the pre-ranked set, so connector/anchor routes (incl. the
            // native-ETH sentinel) survive the spot×depth truncation. Discovery failure is not
            // fatal — the pre-ranked set already guarantees a route.
            let anchor_tokens = derive_anchor_tokens(graph);
            let bounded = find_candidate_paths(
                graph,
                &view,
                order,
                CandidateSearchConfig {
                    min_hops: self.min_hops,
                    max_hops: self.max_hops,
                    max_candidates: BOUNDED_DISCOVERY_CANDIDATES,
                    connector_tokens: self.connector_tokens.as_ref(),
                    anchor_tokens: &anchor_tokens,
                    source_token: order.token_in(),
                    start: &start,
                    timeout_ms,
                },
            );
            match bounded {
                Ok((bounded_paths, _)) => {
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
                // Not fatal — the pre-ranked exhaustive set already guarantees a route — but log it
                // so a misconfiguration or systematic discovery failure is visible rather than
                // silently narrowing the candidate set.
                Err(e) => {
                    debug!(error = %e, "water-fill bounded discovery failed; using exhaustive candidates only")
                }
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
            match MostLiquidAlgorithm::simulate_path(
                path,
                &market,
                token_prices.as_ref(),
                amount_in.clone(),
            ) {
                Ok(result) => {
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
                // A path that can't take the whole order (e.g. concentrated liquidity exhausted at
                // the full amount) may still be worth a fraction in a split, so keep it as a
                // split-only candidate ranked last (gross 0). It never becomes the single-path
                // baseline, and the allocation passes skip it at any chunk it still fails.
                Err(_) => full_outputs.push((idx, BigUint::zero())),
            }
        }

        // No early exit on a missing single path: a split across thin pools can fill an order that
        // no single path can, so the caller decides — it only errors when neither a single path nor
        // a split candidate fills the order.
        full_outputs.sort_by(|(_, a), (_, b)| b.cmp(a));
        let ordered: Vec<Path<DepthAndPrice>> = full_outputs
            .into_iter()
            .map(|(idx, _)| paths[idx].clone())
            .collect();

        debug!(
            candidate_paths = ordered.len(),
            elapsed_ms = start.elapsed().as_millis(),
            "water-fill discovery + full-amount ranking"
        );
        Ok(SetupResult { ordered, market, gas_price, best_single, token_prices })
    }
}

impl Algorithm for WaterFillAlgorithm {
    type GraphType = RoutingGraph<DepthAndPrice>;
    type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        "water_fill"
    }

    #[instrument(level = "debug", skip_all, fields(order_id = %order.id()))]
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

        let SetupResult { ordered, market, gas_price, best_single, token_prices } = self
            .setup(graph, market, label, derived, order, start)
            .await?;
        let token_out = order.token_out();
        let ctx = SplitContext {
            ordered: &ordered,
            market: &market,
            gas_price: &gas_price,
            token_prices: token_prices.as_ref(),
            order,
            start,
        };

        // Build the split candidates; the best net of them competes with the single path.
        let mut candidates: Vec<SplitCandidate> = Vec::new();
        // One coarse (20-chunk) water-fill over the pool-disjoint paths feeds both the floor split
        // and the refined split, so run it once. It is cheap and always finishes, so a tight
        // timeout cannot cut it off while leaving the single path — a winning split is never lost
        // to the clock.
        let disjoint = Self::select_disjoint(ctx.ordered, self.max_paths);
        let coarse = (disjoint.len() >= 2)
            .then(|| self.disjoint_waterfill(&ctx, &disjoint, COARSE_CHUNKS, true))
            .flatten();
        if let Some(coarse) = coarse.as_deref() {
            // The 20-chunk floor split: exactly the coarse allocation.
            if let Some(c) = self.build_disjoint_legs(&ctx, &disjoint, coarse) {
                candidates.push(c);
            }
            // Finer allocation over the same active set (a bonus; a timeout may cut it off, and
            // then the floor stands).
            if let Some(c) = self.disjoint_refine(&ctx, &disjoint, coarse, FINE_CHUNKS) {
                candidates.push(c);
            }
        }
        // Fill-and-spill: a split that lets paths share a pool and branch at an intermediate token
        // (a tree route), which the pool-disjoint splits cannot express.
        if let Some(c) = self.fillspill_alloc(&ctx, FINE_CHUNKS) {
            candidates.push(c);
        }

        // Pick the best-net result. A split candidate must strictly beat the single-path baseline
        // to win; with no baseline (no single path fills the order) the best split wins outright.
        let candidate_count = candidates.len();
        let baseline_net = best_single
            .as_ref()
            .map(|b| b.net_amount_out().clone());
        let mut best: Option<(BigInt, SplitCandidate)> = None;
        for cand in candidates {
            let net = cand.net(&gas_price, token_prices.as_ref(), token_out);
            let beats = match (&best, &baseline_net) {
                (Some((current, _)), _) => net > *current,
                (None, Some(base)) => net > *base,
                (None, None) => true,
            };
            if beats {
                best = Some((net, cand));
            }
        }
        let split_won = best.is_some();
        debug!(
            candidate_count,
            split_won,
            elapsed_ms = start.elapsed().as_millis(),
            "water-fill selected {}",
            if split_won { "split candidate" } else { "single path" }
        );
        match best {
            Some((net, cand)) => Ok(RouteResult::new(cand.route, net, gas_price)),
            // No split won: return the single path if there is one, else nothing fills the order.
            None => best_single.ok_or(AlgorithmError::InsufficientLiquidity),
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

impl WaterFillAlgorithm {
    /// Refines the 20-chunk floor split on a finer grid. Given the shared coarse water-fill the
    /// floor is built from, it fixes the active path set (the coarse-gated paths with a nonzero
    /// amount), re-allocates over that set on a fine grid with the gate off (gas already
    /// justified), then runs the exchange-refinement pass. If a tight timeout cuts off either pass
    /// this returns `None` and the caller falls back to the 20-chunk floor candidate.
    fn disjoint_refine(
        &self,
        ctx: &SplitContext,
        disjoint: &[usize],
        coarse: &[BigUint],
        fine_chunks: usize,
    ) -> Option<SplitCandidate> {
        // The active set is the coarse-gated paths that were allocated a nonzero amount.
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

        // Fine water-fill over the fixed active set, no gate (gas already justified).
        let fine = self.disjoint_waterfill(ctx, &active, fine_chunks, false)?;

        // Exchange refinement. The fine water-fill quantizes each path to a whole chunk, so it can
        // sit up to one chunk off the equal-marginal optimum. Nudge flow between paths at sub-chunk
        // resolution, accepting only strictly-improving moves (never-lose).
        let refined = self.disjoint_exchange(ctx, &active, fine_chunks, fine);
        self.build_disjoint_legs(ctx, &active, &refined)
    }

    /// Net output (gross output minus gas cost in output-token terms) of `path` simulated in
    /// isolation at `amount`. Pool-disjoint paths never interfere, so an isolated re-simulation is
    /// exact. A zero amount means the path is dropped from the route: it yields no output and,
    /// since it is no longer swapped, no gas — so dropping a donor credits its saved gas
    /// automatically.
    fn path_net(
        ctx: &SplitContext,
        path: &Path<DepthAndPrice>,
        amount: &BigUint,
    ) -> Option<BigInt> {
        if amount.is_zero() {
            return Some(BigInt::zero());
        }
        let empty: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let step = Self::simulate_step(path, ctx.market, &empty, amount.clone())?;
        let activation = Self::activation_cost(ctx, &step.gas);
        Some(BigInt::from(step.amount_out) - activation)
    }

    /// Exchange-refinement pass over the fixed active set, starting from the fine water-fill split.
    /// Water-fill can never un-commit a chunk, so its split is only accurate to one fine chunk and
    /// can miss the equal-marginal split. This shifts `delta` of input from an over-allocated donor
    /// to an under-allocated recipient whenever the pair's summed net output strictly improves,
    /// then halves `delta` once no move helps, down to a sub-chunk floor. Paths are pool-disjoint,
    /// so a trial re-simulates only the two paths it touches (unchanged paths keep their cached
    /// net). Only strictly-improving moves are accepted, so the result never scores below the split
    /// it started from.
    fn disjoint_exchange(
        &self,
        ctx: &SplitContext,
        active: &[usize],
        fine_chunks: usize,
        alloc: Vec<BigUint>,
    ) -> Vec<BigUint> {
        let path_count = active.len();
        if path_count < 2 {
            return alloc;
        }
        let amount_in = ctx.order.amount().clone();
        let fine_chunks = fine_chunks.max(1);
        let mut delta = &amount_in / fine_chunks;
        let min_delta = &amount_in / (fine_chunks * EXCHANGE_DELTA_FLOOR);
        if delta.is_zero() {
            return alloc;
        }
        let timeout_ms = self.timeout.as_millis() as u64;

        let mut cum = alloc;
        // Cache each active path's net at its current cumulative amount so a pair trial only
        // re-simulates the two paths it moves flow between, not the whole active set.
        let mut net_cache: Vec<BigInt> = Vec::with_capacity(path_count);
        for (i, &path_idx) in active.iter().enumerate() {
            let Some(net) = Self::path_net(ctx, &ctx.ordered[path_idx], &cum[i]) else {
                // The starting split does not simulate cleanly; refining it is unsafe, so keep it.
                return cum;
            };
            net_cache.push(net);
        }

        let mut sims = 0usize;
        while delta >= min_delta && !delta.is_zero() {
            if ctx.start.elapsed().as_millis() as u64 > timeout_ms || sims >= EXCHANGE_MAX_SIMS {
                break;
            }

            let mut best: Option<ExchangeMove> = None;
            for donor in 0..path_count {
                if sims >= EXCHANGE_MAX_SIMS {
                    break;
                }
                if cum[donor] < delta {
                    continue;
                }
                let donor_amt = &cum[donor] - &delta;
                let Some(donor_net) = Self::path_net(ctx, &ctx.ordered[active[donor]], &donor_amt)
                else {
                    continue;
                };
                sims += 1;
                for recipient in 0..path_count {
                    if recipient == donor || sims >= EXCHANGE_MAX_SIMS {
                        continue;
                    }
                    let recip_amt = &cum[recipient] + &delta;
                    let Some(recip_net) =
                        Self::path_net(ctx, &ctx.ordered[active[recipient]], &recip_amt)
                    else {
                        continue;
                    };
                    sims += 1;
                    let before = &net_cache[donor] + &net_cache[recipient];
                    let after = &donor_net + &recip_net;
                    if after <= before {
                        continue;
                    }
                    let gain = after - before;
                    if best
                        .as_ref()
                        .map(|m| gain > m.gain)
                        .unwrap_or(true)
                    {
                        best = Some(ExchangeMove {
                            donor,
                            recipient,
                            donor_net: donor_net.clone(),
                            recip_net,
                            gain,
                        });
                    }
                }
            }

            let Some(mv) = best else {
                delta = &delta / 2usize;
                continue;
            };
            cum[mv.donor] = &cum[mv.donor] - &delta;
            cum[mv.recipient] = &cum[mv.recipient] + &delta;
            net_cache[mv.donor] = mv.donor_net;
            net_cache[mv.recipient] = mv.recip_net;
        }
        cum
    }

    /// Simulates `amount` through `path`, reading and committing pool states via `overrides`, and
    /// returns the allocation the route assembly consumes.
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
                .get_amount_out_guarded(current.clone(), token_in, token_out)
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
        ctx: &SplitContext,
        allocations: &[PathAllocation],
    ) -> Option<SplitCandidate> {
        let route = build_split_route(allocations, ctx.market, ctx.order).ok()?;
        let token_out = ctx.order.token_out();
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
        ctx: &SplitContext,
        subset: &[usize],
        alloc: &[BigUint],
    ) -> Option<SplitCandidate> {
        let amount_in = ctx.order.amount().clone();
        let mut allocations = Vec::new();
        for (i, &path_idx) in subset.iter().enumerate() {
            if alloc[i].is_zero() {
                continue;
            }
            // Fresh overrides per leg: legs are pool-disjoint, but a pool reused within one path
            // must still see its own first swap.
            let mut overrides: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
            let allocation = Self::allocation_commit(
                &ctx.ordered[path_idx],
                ctx.market,
                &mut overrides,
                alloc[i].clone(),
                ratio(&alloc[i], &amount_in),
            )?;
            allocations.push(allocation);
        }
        if allocations.is_empty() {
            return None;
        }
        Self::candidate_from_allocations(ctx, &allocations)
    }

    /// Incremental water-fill over a set of pool-disjoint paths. Returns the amount allocated to
    /// each path in `subset` order. With `gate`, a path only activates when its first chunk covers
    /// its gas; without it, every path is eligible (used once the active set is fixed).
    fn disjoint_waterfill(
        &self,
        ctx: &SplitContext,
        subset: &[usize],
        num_chunks: usize,
        gate: bool,
    ) -> Option<Vec<BigUint>> {
        let amount_in = ctx.order.amount().clone();
        let num_chunks = num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            return None;
        }
        let remainder = &amount_in - &base_chunk * num_chunks;
        let timeout_ms = self.timeout.as_millis() as u64;
        let path_count = subset.len();

        let mut committed: Vec<HashMap<ComponentId, Box<dyn ProtocolSim>>> = (0..path_count)
            .map(|_| HashMap::new())
            .collect();
        let mut cum_in: Vec<BigUint> = vec![BigUint::zero(); path_count];
        let mut activated: Vec<bool> = vec![!gate; path_count];

        for chunk_idx in 0..num_chunks {
            if ctx.start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };

            let mut best: Option<(usize, BigInt, StepResult)> = None;
            for (i, &path_idx) in subset.iter().enumerate() {
                let Some(step) = Self::simulate_step(
                    &ctx.ordered[path_idx],
                    ctx.market,
                    &committed[i],
                    chunk.clone(),
                ) else {
                    continue;
                };
                let gross_marginal = BigInt::from(step.amount_out.clone());
                let net_marginal = if activated[i] {
                    gross_marginal
                } else {
                    let activation = Self::activation_cost(ctx, &step.gas);
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
    fn select_shared_candidates(&self, ctx: &SplitContext) -> Vec<usize> {
        let mut candidates: Vec<usize> = (0..ctx.ordered.len().min(SHARED_FULL_PATHS)).collect();
        let first_chunk = ctx.order.amount() / COARSE_CHUNKS;
        if first_chunk.is_zero() {
            return candidates;
        }
        let timeout_ms = self.timeout.as_millis() as u64;
        let empty: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let mut marginal: Vec<(usize, BigInt)> = Vec::new();
        for (idx, path) in ctx
            .ordered
            .iter()
            .enumerate()
            .take(SHARED_MARGIN_PROBE_PATHS)
        {
            if ctx.start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let Some(step) = Self::simulate_step(path, ctx.market, &empty, first_chunk.clone())
            else {
                continue;
            };
            let activation = Self::activation_cost(ctx, &step.gas);
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
    fn fillspill_alloc(&self, ctx: &SplitContext, fine_chunks: usize) -> Option<SplitCandidate> {
        let candidates = self.select_shared_candidates(ctx);
        if candidates.len() < 2 {
            return None;
        }

        // Phase 1: coarse gated pass to choose the active candidate set.
        let (coarse_counts, _) = self.fillspill_waterfill(ctx, &candidates, COARSE_CHUNKS, true)?;
        let active: Vec<usize> = candidates
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
        let (_, schedule) = self.fillspill_waterfill(ctx, &active, fine_chunks, false)?;
        if schedule.is_empty() {
            return None;
        }

        self.build_fillspill_route(ctx, &active, &schedule)
    }

    /// Incremental fill-and-spill water-fill over a single shared overlay. Returns the chunk count
    /// each candidate received and the ordered commit schedule of `(active_index, chunk_amount)`.
    #[allow(clippy::type_complexity)]
    fn fillspill_waterfill(
        &self,
        ctx: &SplitContext,
        subset: &[usize],
        num_chunks: usize,
        gate: bool,
    ) -> Option<(Vec<usize>, Vec<(usize, BigUint)>)> {
        let amount_in = ctx.order.amount().clone();
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
            if ctx.start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };

            let mut best: Option<(usize, BigInt, StepResult)> = None;
            for (i, &path_idx) in subset.iter().enumerate() {
                if !activated[i] && active_count >= self.max_paths {
                    continue;
                }
                let Some(step) = Self::simulate_step(
                    &ctx.ordered[path_idx],
                    ctx.market,
                    &overlay,
                    chunk.clone(),
                ) else {
                    continue;
                };
                let gross_marginal = BigInt::from(step.amount_out.clone());
                let net_marginal = if activated[i] {
                    gross_marginal
                } else {
                    let activation = Self::activation_cost(ctx, &step.gas);
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
        ctx: &SplitContext,
        active: &[usize],
        schedule: &[(usize, BigUint)],
    ) -> Option<SplitCandidate> {
        let amount_in = ctx.order.amount().clone();
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
                &ctx.ordered[active[i]],
                ctx.market,
                &mut overrides,
                cand_in[i].clone(),
                ratio(&cand_in[i], &amount_in),
            )?;
            allocations.push(allocation);
        }
        Self::candidate_from_allocations(ctx, &allocations)
    }
}

/// A path's identity for dedup: its ordered component-id sequence.
fn path_key<W>(path: &Path<'_, W>) -> Vec<ComponentId> {
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

// ==================== Candidate discovery ====================
//
// Bounded, amount-aware frontier search, combined with the exhaustive enumeration by `setup`. It
// expands from the sell token: simulate frontier edges live and prefer edges into the output token,
// the configured connector-token allowlist, or a set of anchor tokens (a soft ranking hint) derived
// per solve from the graph — the most-connected tokens plus the native-ETH sentinel (see
// `derive_anchor_tokens`). Generic over the graph's edge weight `W`
// (discovery only reads `component_id`s) so it runs on the production `DepthAndPrice` graph while
// tests exercise it on a bare topology graph.

type RankedPathScores = Vec<(usize, BigInt)>;
type CandidatePathSet<'a, W = ()> = (Vec<Path<'a, W>>, RankedPathScores);

#[derive(Clone)]
struct CandidatePathState<'a, W> {
    node: NodeIndex,
    path: Path<'a, W>,
    amount_out: BigUint,
}

struct ScoredEdge<'a, W> {
    target: NodeIndex,
    edge: &'a EdgeData<W>,
    amount_out: BigUint,
    priority: u8,
}

/// Parameters for one bounded candidate-discovery run.
#[derive(Clone, Copy)]
struct CandidateSearchConfig<'a> {
    min_hops: usize,
    max_hops: usize,
    max_candidates: usize,
    connector_tokens: Option<&'a HashSet<Address>>,
    anchor_tokens: &'a HashSet<Address>,
    source_token: &'a Address,
    start: &'a Instant,
    timeout_ms: u64,
}

fn timed_out(start: &Instant, timeout_ms: u64) -> bool {
    start.elapsed().as_millis() as u64 > timeout_ms
}

/// Runs the bounded discovery and returns the candidate paths plus their `(index, full-amount
/// gross output)` ranking, best first.
fn find_candidate_paths<'a, W>(
    graph: &'a RoutingGraph<W>,
    market: &MarketDataView<'_>,
    order: &Order,
    cfg: CandidateSearchConfig<'_>,
) -> Result<CandidatePathSet<'a, W>, AlgorithmError>
where
    W: Clone,
{
    if cfg.min_hops == 0 || cfg.min_hops > cfg.max_hops {
        return Err(AlgorithmError::InvalidConfiguration {
            reason: format!(
                "invalid hop configuration: min_hops={} max_hops={}",
                cfg.min_hops, cfg.max_hops,
            ),
        });
    }
    let from_idx =
        find_token_node(graph, order.token_in(), NoPathReason::SourceTokenNotInGraph, order)?;
    let to_idx =
        find_token_node(graph, order.token_out(), NoPathReason::DestinationTokenNotInGraph, order)?;

    let mut found = Vec::new();
    let mut frontier = vec![CandidatePathState {
        node: from_idx,
        path: Path::new(),
        amount_out: order.amount().clone(),
    }];

    for _depth in 0..cfg.max_hops {
        if timed_out(cfg.start, cfg.timeout_ms) || frontier.is_empty() {
            break;
        }
        let mut next_by_node: HashMap<NodeIndex, Vec<CandidatePathState<'a, W>>> = HashMap::new();
        for state in frontier {
            if state.node == to_idx && from_idx != to_idx {
                continue;
            }
            expand_candidate_state(
                graph,
                market,
                &cfg,
                to_idx,
                state,
                &mut found,
                &mut next_by_node,
            );
        }
        frontier = prune_candidate_frontier(next_by_node);
    }

    rank_found_candidate_paths(found, cfg.max_candidates, order)
}

fn find_token_node<W>(
    graph: &RoutingGraph<W>,
    token: &Address,
    reason: NoPathReason,
    order: &Order,
) -> Result<NodeIndex, AlgorithmError> {
    graph
        .node_index(token)
        .ok_or(AlgorithmError::NoPath {
            from: order.token_in().clone(),
            to: order.token_out().clone(),
            reason,
        })
}

fn expand_candidate_state<'a, W>(
    graph: &'a RoutingGraph<W>,
    market: &MarketDataView<'_>,
    cfg: &CandidateSearchConfig<'_>,
    target: NodeIndex,
    state: CandidatePathState<'a, W>,
    found: &mut Vec<(Path<'a, W>, BigUint)>,
    next_by_node: &mut HashMap<NodeIndex, Vec<CandidatePathState<'a, W>>>,
) where
    W: Clone,
{
    let edges = candidate_edges_for_state(graph, market, cfg, target, &state);
    for candidate in edges {
        if timed_out(cfg.start, cfg.timeout_ms) {
            break;
        }
        let mut path = state.path.clone();
        path.add_hop(&graph[state.node], candidate.edge, &graph[candidate.target]);
        let path_state = CandidatePathState {
            node: candidate.target,
            path: path.clone(),
            amount_out: candidate.amount_out,
        };
        if candidate.target == target && path.len() >= cfg.min_hops {
            found.push((path.clone(), path_state.amount_out.clone()));
        }
        if path.len() < cfg.max_hops {
            next_by_node
                .entry(candidate.target)
                .or_default()
                .push(path_state);
        }
    }
}

fn candidate_edges_for_state<'a, W>(
    graph: &'a RoutingGraph<W>,
    market: &MarketDataView<'_>,
    cfg: &CandidateSearchConfig<'_>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
) -> Vec<ScoredEdge<'a, W>> {
    let mut preferred = score_candidate_edges(graph, market, cfg, target, state, true);
    if preferred.is_empty() {
        preferred = score_candidate_edges(graph, market, cfg, target, state, false);
    }
    select_candidate_edges(preferred, CANDIDATE_EDGES_PER_STATE)
}

fn score_candidate_edges<'a, W>(
    graph: &'a RoutingGraph<W>,
    market: &MarketDataView<'_>,
    cfg: &CandidateSearchConfig<'_>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
    preferred_only: bool,
) -> Vec<ScoredEdge<'a, W>> {
    let mut scored = Vec::new();
    for edge in graph.edges(state.node) {
        let next_node = edge.target();
        if !can_extend_path(graph, state, next_node, target, edge.weight(), cfg) {
            continue;
        }
        let priority = match candidate_priority(graph, next_node, target, cfg) {
            Some(priority) => priority,
            None if preferred_only => continue,
            None => 3,
        };
        let Some(amount_out) = simulate_edge(
            market,
            &state.amount_out,
            &graph[state.node],
            edge.weight(),
            &graph[next_node],
        ) else {
            continue;
        };
        scored.push(ScoredEdge { target: next_node, edge: edge.weight(), amount_out, priority });
    }
    scored
}

/// Derives bounded discovery's soft anchor set from the live graph: the `DERIVED_ANCHOR_COUNT`
/// most-connected tokens (highest pool-edge degree) plus the native-ETH sentinel. Degree is the
/// same connectivity signal `derive-connector-tokens` ranks by, so anchoring stays correct on any
/// chain without a hardcoded per-chain list. The native-ETH zero address carries near-zero degree
/// but is load-bearing for `WETH → ETH → token` routes where Tycho models native ETH as `0x0`, so
/// it is anchored explicitly.
fn derive_anchor_tokens<W>(graph: &RoutingGraph<W>) -> HashSet<Address> {
    let mut anchors: HashSet<Address> = graph
        .most_connected_tokens(DERIVED_ANCHOR_COUNT)
        .iter()
        .cloned()
        .collect();
    anchors.insert(Address::from([0u8; 20]));
    anchors
}

fn candidate_priority<W>(
    graph: &RoutingGraph<W>,
    node: NodeIndex,
    target: NodeIndex,
    cfg: &CandidateSearchConfig<'_>,
) -> Option<u8> {
    if node == target {
        return Some(0);
    }
    let token = &graph[node];
    match cfg.connector_tokens {
        Some(tokens) => tokens.contains(token).then_some(1),
        None => cfg
            .anchor_tokens
            .contains(token)
            .then_some(2),
    }
}

fn can_extend_path<W>(
    graph: &RoutingGraph<W>,
    state: &CandidatePathState<'_, W>,
    next_node: NodeIndex,
    target: NodeIndex,
    edge: &EdgeData<W>,
    cfg: &CandidateSearchConfig<'_>,
) -> bool {
    let next_addr = &graph[next_node];
    if state
        .path
        .edge_iter()
        .iter()
        .any(|existing| existing.component_id == edge.component_id)
    {
        return false;
    }
    if state.path.tokens.contains(&next_addr) {
        return false;
    }
    if next_addr == cfg.source_token {
        return false;
    }
    if next_node == target {
        return true;
    }
    cfg.connector_tokens
        .map(|tokens| tokens.contains(next_addr))
        .unwrap_or(true)
}

fn simulate_edge<W>(
    market: &MarketDataView<'_>,
    amount: &BigUint,
    token_in_addr: &Address,
    edge: &EdgeData<W>,
    token_out_addr: &Address,
) -> Option<BigUint> {
    let token_in = market.get_token(token_in_addr)?;
    let token_out = market.get_token(token_out_addr)?;
    let state = market.get_simulation_state(&edge.component_id)?;
    state
        .get_amount_out_guarded(amount.clone(), token_in, token_out)
        .ok()
        .map(|result| result.amount)
}

fn select_candidate_edges<W>(
    mut scored: Vec<ScoredEdge<'_, W>>,
    max_edges: usize,
) -> Vec<ScoredEdge<'_, W>> {
    scored.sort_by(compare_scored_edges);
    let mut selected = Vec::new();
    let mut per_target: HashMap<NodeIndex, usize> = HashMap::new();
    for edge in scored {
        let limit = if edge.priority == 0 {
            CANDIDATE_DIRECT_EDGES_PER_TOKEN
        } else {
            CANDIDATE_CONNECTOR_EDGES_PER_TOKEN
        };
        let count = per_target
            .entry(edge.target)
            .or_default();
        if *count >= limit {
            continue;
        }
        *count += 1;
        selected.push(edge);
        if selected.len() >= max_edges {
            break;
        }
    }
    selected
}

fn compare_scored_edges<W>(a: &ScoredEdge<'_, W>, b: &ScoredEdge<'_, W>) -> Ordering {
    a.priority
        .cmp(&b.priority)
        .then_with(|| b.amount_out.cmp(&a.amount_out))
}

fn prune_candidate_frontier<W>(
    by_node: HashMap<NodeIndex, Vec<CandidatePathState<'_, W>>>,
) -> Vec<CandidatePathState<'_, W>> {
    by_node
        .into_values()
        .flat_map(|mut states| {
            states.sort_by(|a, b| b.amount_out.cmp(&a.amount_out));
            states.truncate(CANDIDATE_STATES_PER_NODE);
            states
        })
        .collect()
}

fn rank_found_candidate_paths<'a, W>(
    mut found: Vec<(Path<'a, W>, BigUint)>,
    max_candidates: usize,
    order: &Order,
) -> Result<CandidatePathSet<'a, W>, AlgorithmError> {
    found.sort_by(|(_, a), (_, b)| b.cmp(a));
    let mut keys = HashSet::new();
    let mut paths = Vec::new();
    let mut scores = Vec::new();

    for (path, amount_out) in found {
        if !keys.insert(path_key(&path)) {
            continue;
        }
        let idx = paths.len();
        paths.push(path);
        scores.push((idx, BigInt::from(amount_out)));
        if paths.len() >= max_candidates {
            break;
        }
    }

    if paths.is_empty() {
        return Err(AlgorithmError::NoPath {
            from: order.token_in().clone(),
            to: order.token_out().clone(),
            reason: NoPathReason::NoGraphPath,
        });
    }
    Ok((paths, scores))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use num_traits::ToPrimitive;
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

    use super::*;
    use crate::{
        algorithm::{
            split_test_harness::{
                evaluate_scenario, optimal_two_pool_output, split_metrics, split_scenarios,
                two_equal_weth_usdc, TWO_EQUAL_USDC_RESERVE, TWO_EQUAL_WETH_RESERVE,
            },
            test_utils::{
                addr, setup_market_unweighted, setup_market_weighted_boxed, token_with_decimals,
                ConstantProductSim, DivByZeroSim,
            },
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

    fn v2_pool(reserve_a: u128, reserve_b: u128) -> UniswapV2State {
        UniswapV2State::new(
            U256::from(reserve_a) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(reserve_b) * U256::from(10u64).pow(U256::from(18u64)),
        )
    }

    // ==================== Portfolio behavior ====================

    fn water_fill_default() -> WaterFillAlgorithm {
        WaterFillAlgorithm::with_config(
            AlgorithmConfig::new(1, 4, Duration::from_millis(5000), None).unwrap(),
        )
        .unwrap()
    }

    /// Across every shared split scenario: never lose to the best single path, land within 5% of
    /// the analytical optimum, and return a structurally valid route.
    #[tokio::test]
    async fn test_water_fill_all_scenarios() {
        let algo = water_fill_default();
        for scenario in split_scenarios::all() {
            let name = scenario.name;
            let (market, gm) = scenario.build_market_weighted();
            let result = evaluate_scenario(&algo, &scenario, market, gm).await;

            result.assert_passes_lower_bound();
            // water_fill reaches the analytical optimum within 5% on single-hop and shared-prefix
            // scenarios. DOUBLE_SPLIT re-splits at both hops with mismatched pool sizes; the
            // token-merged disjoint/fill-and-spill allocation reaches ~90% of that cross-hop
            // optimum -- still well above the single-path floor, just short of PFW's per-hop
            // line search there.
            let tolerance_pct = if name == "DOUBLE_SPLIT" { 10 } else { 5 };
            assert!(
                result.within_pct_of_optimum(tolerance_pct),
                "'{name}': not within {tolerance_pct}% of optimum",
            );
            let route = result
                .route
                .as_ref()
                .unwrap_or_else(|| panic!("'{name}': expected a route"));
            assert!(route.validate().is_ok(), "'{name}': route validation failed");
        }
    }

    /// When the extra-hop gas exceeds the split's gross benefit, water-fill returns a single path
    /// rather than a net-negative split.
    #[tokio::test]
    async fn test_water_fill_gas_kills_split() {
        let scenario = split_scenarios::gas_kills_split();
        let (market, gm) = scenario.build_market_weighted();
        let result = evaluate_scenario(&water_fill_default(), &scenario, market, gm).await;
        assert_eq!(result.path_count, 1, "high gas should prevent splitting");
    }

    /// A split fills an order that no single path can. Two parallel constant-product pools each
    /// revert once the input reaches their reserve, so the full order overruns either pool alone;
    /// split in half it fits both. The portfolio returns the split even though the single-path
    /// baseline is absent — instead of the earlier `InsufficientLiquidity` short-circuit.
    #[tokio::test]
    async fn test_split_fills_when_no_single_path_can() {
        let a = token_with_decimals(0x01, "A", 18);
        let b = token_with_decimals(0x02, "B", 18);
        // reserve_0 (token A, the input side) = 800; a swap reverts once its input reaches it. The
        // full order (1000 A) overruns either pool, but ~500 A per pool in a 50/50 split fits.
        let pool = || {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(800u64) * BigUint::from(10u64).pow(18),
                reserve_1: BigUint::from(10_000u64) * BigUint::from(10u64).pow(18),
                gas: 50_000,
            }) as Box<dyn ProtocolSim>
        };
        let (market, gm) = setup_market_weighted_boxed(vec![
            ("pool_x", &a, &b, pool()),
            ("pool_y", &a, &b, pool()),
        ]);
        let order = Order::new(
            a.address.clone(),
            b.address.clone(),
            BigUint::from(1_000u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        // Premise: no single path fills the full order.
        let single = MostLiquidAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(gm.graph(), market.clone(), None, None, &order)
            .await;
        assert!(single.is_err(), "premise: no single path fills the full order");

        // The portfolio still returns a split across both pools.
        let split = WaterFillAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(gm.graph(), market.clone(), None, None, &order)
            .await
            .expect("water_fill returns a split when no single path fills");
        assert!(split.route().swaps().len() >= 2, "expected a split across both pools");
    }

    /// A pool whose math panics mid-simulation must be skipped like any failing pool, not
    /// unwind through the solver worker thread. The panicking pool advertises huge depth so
    /// discovery ranks it first and the bulk fill loops actually simulate it.
    #[tokio::test]
    async fn test_water_fill_contains_simulation_panic() {
        let a = token_with_decimals(0x01, "A", 18);
        let b = token_with_decimals(0x02, "B", 18);
        let healthy = Box::new(ConstantProductSim {
            reserve_0: BigUint::from(10_000u64) * BigUint::from(10u64).pow(18),
            reserve_1: BigUint::from(10_000u64) * BigUint::from(10u64).pow(18),
            gas: 50_000,
        }) as Box<dyn ProtocolSim>;
        let (market, gm) = setup_market_weighted_boxed(vec![
            ("pool_ok", &a, &b, healthy),
            ("pool_panics", &a, &b, Box::new(DivByZeroSim::default())),
        ]);
        let order = Order::new(
            a.address.clone(),
            b.address.clone(),
            BigUint::from(100u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        let result = WaterFillAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(gm.graph(), market.clone(), None, None, &order)
            .await
            .expect("panicking pool is skipped; the healthy pool still fills the order");

        assert!(
            result
                .route()
                .swaps()
                .iter()
                .all(|swap| swap.component_id() == "pool_ok"),
            "route must only use the healthy pool",
        );
    }

    /// On two equal fee-free pools the split's gross output must come within a tight tolerance of
    /// the analytical two-pool optimum (a 50/50 split): the fine 256-chunk allocation finds the
    /// optimal split, not just any splitting one.
    #[tokio::test]
    async fn test_water_fill_output_near_two_pool_optimum() {
        let m = two_equal_weth_usdc(1);
        let trade = 500u64;
        let order = whole_weth_order(&m.weth, &m.usdc, trade);

        let result = WaterFillAlgorithm::with_config(config())
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
        let (_, path_count, gross) = split_metrics(&result, &m.weth, &m.usdc);
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
            "split gross {gross} should be within 0.1% of the two-pool optimum {optimum}",
        );
    }

    /// Under a tight timeout the split must never lose to the best single path: the 20-chunk split
    /// does the cheap coarse allocation, so cutting off later refinement cannot drop the result
    /// below the single path. A budget too tight to finish yields no route at all (the router falls
    /// back to other pools) — a no-answer, not a loss — so the never-lose check runs only for
    /// budgets where water-fill returns a route. The 50ms budget comfortably exceeds this
    /// scenario's few-ms solve, so the comparison is always exercised.
    #[tokio::test]
    async fn test_water_fill_no_loss_under_tight_timeout() {
        for ms in [1u64, 5, 50] {
            let m = two_equal_weth_usdc(1_000_000_000);
            let order = whole_weth_order(&m.weth, &m.usdc, 500);

            let split = WaterFillAlgorithm::with_config(config_ms(ms))
                .unwrap()
                .find_best_route(
                    m.weighted.graph(),
                    m.market.clone(),
                    None,
                    Some(m.derived.clone()),
                    &order,
                )
                .await;
            let single = MostLiquidAlgorithm::with_config(config_ms(ms))
                .unwrap()
                .find_best_route(
                    m.weighted.graph(),
                    m.market.clone(),
                    None,
                    Some(m.derived.clone()),
                    &order,
                )
                .await;

            let (Ok(split), Ok(single)) = (split, single) else {
                continue;
            };
            let (split_net, _, _) = split_metrics(&split, &m.weth, &m.usdc);
            let (single_net, _, _) = split_metrics(&single, &m.weth, &m.usdc);
            assert!(
                split_net >= single_net,
                "split lost to single-path under {ms}ms timeout: split={split_net} single={single_net}",
            );
        }
    }

    // ==================== Candidate discovery ====================

    /// Bounded discovery finds both parallel pools as candidate paths and ranks the deeper pool
    /// first by simulated full-amount output, using live simulation only (no precomputed edge
    /// weights on the weightless graph).
    #[tokio::test]
    async fn test_discovery_finds_and_ranks_parallel_pools() {
        let link = token_with_decimals(0x01, "LINK", 18);
        let weth = token_with_decimals(0x02, "WETH", 18);
        let (market, graph_manager) = setup_market_unweighted(vec![
            (
                "a_weak_link_weth",
                &link,
                &weth,
                Box::new(v2_pool(2_000_000, 264)) as Box<dyn ProtocolSim>,
            ),
            (
                "z_strong_link_weth",
                &link,
                &weth,
                Box::new(v2_pool(2_000_000, 5_700)) as Box<dyn ProtocolSim>,
            ),
        ]);
        let order = Order::new(
            link.address.clone(),
            weth.address.clone(),
            BigUint::from(1_000u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        let start = Instant::now();
        let view = market.read().await;
        let (paths, scores) = find_candidate_paths(
            graph_manager.graph(),
            &view,
            &order,
            CandidateSearchConfig {
                min_hops: 1,
                max_hops: 3,
                max_candidates: 128,
                connector_tokens: None,
                anchor_tokens: &HashSet::new(),
                source_token: order.token_in(),
                start: &start,
                timeout_ms: 2000,
            },
        )
        .expect("discovery finds candidates");

        assert_eq!(paths.len(), 2, "both parallel pools should be discovered");
        // Scores are (path index, full-amount gross output), best first: the deeper pool wins.
        let best_path = &paths[scores[0].0];
        assert_eq!(
            best_path.edge_iter()[0].component_id,
            "z_strong_link_weth",
            "discovery should rank by simulated output, not topology or edge weights",
        );
    }

    /// An invalid hop configuration is rejected before any graph work.
    #[tokio::test]
    async fn test_discovery_rejects_invalid_hop_configuration() {
        let link = token_with_decimals(0x01, "LINK", 18);
        let weth = token_with_decimals(0x02, "WETH", 18);
        let (market, graph_manager) = setup_market_unweighted(vec![(
            "link_weth",
            &link,
            &weth,
            Box::new(v2_pool(2_000_000, 5_700)) as Box<dyn ProtocolSim>,
        )]);
        let order = Order::new(
            link.address.clone(),
            weth.address.clone(),
            BigUint::from(1_000u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        let start = Instant::now();
        let view = market.read().await;
        let result = find_candidate_paths(
            graph_manager.graph(),
            &view,
            &order,
            CandidateSearchConfig {
                min_hops: 0,
                max_hops: 3,
                max_candidates: 128,
                connector_tokens: None,
                anchor_tokens: &HashSet::new(),
                source_token: order.token_in(),
                start: &start,
                timeout_ms: 2000,
            },
        );
        assert!(
            matches!(result, Err(AlgorithmError::InvalidConfiguration { .. })),
            "min_hops of 0 should be rejected",
        );
    }

    /// Anchors are the most-connected tokens (highest pool-edge degree) plus the native-ETH
    /// sentinel, derived from the graph rather than a hardcoded list.
    #[test]
    fn test_derive_anchor_tokens_ranks_hub_and_includes_native_sentinel() {
        let hub = token_with_decimals(0x01, "HUB", 18);
        let a = token_with_decimals(0x02, "A", 18);
        let b = token_with_decimals(0x03, "B", 18);
        let c = token_with_decimals(0x04, "C", 18);
        let (_market, graph_manager) = setup_market_unweighted(vec![
            ("hub_a", &hub, &a, Box::new(v2_pool(1, 1)) as Box<dyn ProtocolSim>),
            ("hub_b", &hub, &b, Box::new(v2_pool(1, 1)) as Box<dyn ProtocolSim>),
            ("hub_c", &hub, &c, Box::new(v2_pool(1, 1)) as Box<dyn ProtocolSim>),
        ]);

        let anchors = derive_anchor_tokens(graph_manager.graph());

        assert!(anchors.contains(&hub.address), "highest-degree token should be anchored");
        assert!(
            anchors.contains(&Address::from([0u8; 20])),
            "native-ETH sentinel should always be anchored",
        );
    }
}
