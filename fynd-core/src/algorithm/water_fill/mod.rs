//! Water-fill split-routing algorithm (`water_fill`).
//!
//! For large orders, price impact makes it better to split the order across several parallel routes
//! so the marginal price stays low. `WaterFillAlgorithm` builds up to four candidate routes and
//! returns the one with the best output net of gas, never worse than the best single path.
//!
//! The four candidates are the best single path plus three ways to split it:
//!
//! * **20-chunk disjoint split** — splits across paths that share no component, in 20 chunks. This
//!   is the safety net: it is cheap and always finishes, so a tight timeout cannot cut off the
//!   split while leaving the single path. That is what makes the never-lose guarantee hold under
//!   time pressure.
//! * **256-chunk disjoint split** — the same component-disjoint paths in 256 chunks, in two phases:
//!   first pick which paths to use at coarse (20-chunk) granularity, where each path's share is
//!   large enough for its gas-activation gate to be meaningful; then allocate across the chosen
//!   paths at fine (256-chunk) granularity with the gate off, since their gas is already covered.
//! * **Fill-and-spill** — a split that lets paths share a component and branch at an intermediate
//!   token (a tree route), which the component-disjoint splits cannot express.
//!
//! Every split fills the order in chunks: for each chunk, it checks how much each path returns for
//! that chunk and gives the chunk to the best one. Because constant-product / tick AMMs are
//! path-independent in cumulative input (one swap of `x` equals two back-to-back swaps summing to
//! `x`), it simulates only the next chunk against the component state committed so far — O(chunks)
//! instead of O(chunks^2). That saved work makes the 256-chunk split affordable.
//!
//! Candidate discovery (the "Candidate discovery" section below) combines an exhaustive path
//! enumeration with a bounded, amount-aware frontier search, so connector and anchor routes
//! (including the native-ETH sentinel) survive the spot × depth cutoff. Every returned route is
//! assembled through the shared split primitives and can be encoded on-chain.

mod config;
mod models;

use std::{
    cmp::{Ordering, Reverse},
    time::{Duration, Instant},
};

use models::{
    CandidatePathState, CandidateSearchConfig, Deadline, Discovery, ExchangeMove,
    FullAmountOutcome, FullAmountRanking, ScoredEdge, SetupResult, SolveInput, SolveStage,
    SplitCandidate, StepResult,
};
use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, instrument, trace};
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim};

use super::{
    most_liquid::DepthAndPrice,
    paths,
    sim_meter::{self, MeteredProtocolSim},
    split_primitives::{
        build_split_route, HopDescriptor, MarketOverrides, PathAllocation, SimulatedHop,
    },
    Algorithm, AlgorithmConfig, NoPathReason,
};
use crate::{
    algorithm::{
        paths::read_market,
        swap_cache::{PoolDirection, Refusal, SwapCache, SwapResult},
        water_fill::config::{
            BASELINE_CANDIDATES, CANDIDATE_CONNECTOR_EDGES_PER_TOKEN,
            CANDIDATE_DIRECT_EDGES_PER_TOKEN, CANDIDATE_EDGES_PER_STATE, CANDIDATE_STATES_PER_NODE,
            COARSE_CHUNKS, DEFAULT_MAX_CANDIDATES, DEFAULT_MAX_PATHS, DERIVED_ANCHOR_COUNT,
            EXCHANGE_DELTA_FLOOR, EXCHANGE_MAX_SIMS, FINE_CHUNKS, MAX_DISCOVERY_CANDIDATES,
            SHARED_FULL_PATHS, SHARED_MARGIN_PATHS, SHARED_MARGIN_PROBE_PATHS,
            SHARED_MAX_CANDIDATES,
        },
    },
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketDataView, MarketState, StateLabel},
    graph::{EdgeData, GraphQueryFilter, Path, TopologyGraph, TopologyGraphManager},
    types::{ComponentId, Order, RouteResult},
    AlgorithmError,
};

/// Splits an order across component-disjoint (and, via fill-and-spill, component-sharing) paths to
/// reduce price impact, returning the best net of the single path and several split allocations.
pub struct WaterFillAlgorithm {
    /// The hop bounds and connector tokens every route search runs under.
    query: GraphQueryFilter,
    timeout: Duration,
    max_candidates: usize,
    max_paths: usize,
}

impl WaterFillAlgorithm {
    /// Creates a `WaterFillAlgorithm` from an `AlgorithmConfig`.
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self {
            query: GraphQueryFilter {
                min_hops: config.min_hops(),
                max_hops: config.max_hops(),
                connector_tokens: config.connector_tokens().cloned(),
            },
            timeout: config.timeout(),
            max_candidates: config
                .max_routes()
                .unwrap_or(DEFAULT_MAX_CANDIDATES)
                .max(DEFAULT_MAX_PATHS),
            max_paths: DEFAULT_MAX_PATHS,
        })
    }

    /// Simulates `amount` through `path`, reading each component from `overlay` if present else the
    /// base state. Returns the output, summed gas, and the resulting per-component states so
    /// the caller can commit them into an overlay.
    fn simulate_step<'g>(
        path: &Path<'g, DepthAndPrice>,
        market: &MarketState,
        overlay: &MarketOverrides,
        amount: BigUint,
    ) -> Option<StepResult> {
        let mut current = amount;
        let mut total_gas = BigUint::zero();
        // A component touched twice in one path must see its own first swap, which needs an
        // intra-path overlay carried across hops. That reuse is rare, so only pay the
        // per-hop state clone when the path actually repeats a component; the common case
        // skips the clone entirely.
        let path_reuses_component = path_reuses_component(path);
        let mut intra_path_states: FxHashMap<ComponentId, Box<dyn ProtocolSim>> =
            FxHashMap::default();
        let mut new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)> =
            Vec::with_capacity(path.len());

        for (address_in, edge, address_out) in path.iter() {
            let token_in = market.get_token(address_in)?;
            let token_out = market.get_token(address_out)?;
            let component_id = &edge.component_id;
            let state = hop_state(market, component_id, Some(&intra_path_states), Some(overlay))?;
            let result = state
                .get_amount_out_metered(
                    component_id,
                    SolveStage::Chunking.label(),
                    current.clone(),
                    token_in,
                    token_out,
                )
                .ok()?;
            total_gas += &result.gas;
            if path_reuses_component {
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
    fn activation_cost(input: &SolveInput, gas: &BigUint) -> BigInt {
        Self::gas_cost_in_token(
            gas,
            &input.gas_price,
            input.token_prices.as_ref(),
            input.order.token_out(),
        )
        .map(BigInt::from)
        .unwrap_or_else(BigInt::zero)
    }

    /// Picks up to `max_paths` paths that share no component, so their outputs can be summed
    /// without re-simulating — two paths through the same component compete for its liquidity,
    /// so their separate outputs would not add up.
    ///
    /// Walks `ranked` best first and keeps a path only if none of its components are already used
    /// by a kept path, skipping it otherwise. Returns the kept paths' indices into `ranked`.
    fn select_disjoint(ranked: &[Path<DepthAndPrice>], max_paths: usize) -> Vec<usize> {
        let mut visited_components: FxHashSet<&ComponentId> = FxHashSet::default();
        let mut selected = Vec::new();
        for (idx, path) in ranked.iter().enumerate() {
            let path_components: Vec<&ComponentId> = path
                .edge_iter()
                .iter()
                .map(|e| &e.component_id)
                .collect();
            if path_components
                .iter()
                .any(|c| visited_components.contains(*c))
            {
                continue;
            }
            for c in path_components {
                visited_components.insert(c);
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
    async fn setup<'o, 'g>(
        &self,
        graph: &'g TopologyGraph<DepthAndPrice>,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &'o Order,
        deadline: Deadline,
    ) -> Result<SetupResult<'o, 'g>, AlgorithmError> {
        let token_prices = if let Some(ref derived) = derived {
            derived
                .read()
                .await
                .token_prices()
                .cloned()
        } else {
            None
        };

        let mut scored_paths = self.top_scored_paths(graph, order)?;
        let mut joined_paths = Vec::new();

        let market_view = read_market(&market, label).await?;

        // Bounded amount-aware discovery (see the discovery section below): union its
        // candidates ahead of the pre-ranked set, so connector/anchor routes (incl. the
        // native-ETH sentinel) survive the spot×depth truncation. Discovery failure is not
        // fatal — the pre-ranked set already guarantees a route.
        let anchor_tokens = derive_anchor_tokens(graph);

        // Discovery and ranking both swap against untouched state, so they share one cache: every
        // frontier edge discovery simulates is an answer ranking would otherwise pay for again.
        // It outlives setup because the allocation passes that read untouched state reuse it too.
        let mut cache = SwapCache::new();
        sim_meter::start_solve();

        let discovered_paths = discover_paths(
                graph,
                &market_view,
                order,
                &mut cache,
                CandidateSearchConfig {
                    query: &self.query,
                    max_candidates: MAX_DISCOVERY_CANDIDATES,
                    anchor_tokens: &anchor_tokens,
                    source_token: order.token_in(),
                    deadline,
                },
            )
            .inspect_err(|e| {
                debug!(error = %e, "water-fill bounded discovery failed; using exhaustive candidates only")
            })
            .unwrap_or_default();

        let mut keys: FxHashSet<Vec<ComponentId>> = scored_paths
            .iter()
            .map(path_key)
            .collect();
        for path in discovered_paths {
            if keys.insert(path_key(&path)) {
                joined_paths.push(path);
            }
        }
        joined_paths.append(&mut scored_paths);

        let component_ids: FxHashSet<&ComponentId> = joined_paths
            .iter()
            .flat_map(|p| {
                p.edge_iter()
                    .iter()
                    .map(|e| &e.component_id)
            })
            .collect();
        let market_state = market_view.extract_subset_with_overlay(&component_ids);
        let gas_price = paths::fetch_gas_price(&market_state)?;
        drop(market_view);

        let amount_in = order.amount().clone();
        // Holds the candidates in enumeration order to start with; ranking reorders them below.
        let mut input = SolveInput {
            ordered: joined_paths,
            market: market_state,
            gas_price,
            token_prices,
            order,
            deadline,
        };
        let ranking = self.rank_at_full_amount(&input, &mut cache);

        // The baseline is the bar every split has to beat, so it is settled on exact figures.
        // Ranking may have read a path's amounts across two nearby ones, which understates and can
        // demote a path that deserved the top place; taking the best of the top few as simulated
        // costs a handful of builds and keeps the bar at the best single path really on offer.
        // Building is what copies a component and a pool state per leg, which is why it is a few
        // and not all of them.
        let build_baseline = |path_ix: usize| {
            paths::simulate_pool_path(
                &input.ordered[path_ix],
                &input.market,
                input.token_prices.as_ref(),
                amount_in.clone(),
            )
            .ok()
        };
        let best_single = ranking
            .by_output_net_gas
            .iter()
            .take(BASELINE_CANDIDATES)
            .filter_map(|&path_ix| build_baseline(path_ix))
            .max_by(|a, b| {
                a.net_amount_out()
                    .cmp(b.net_amount_out())
            })
            .or_else(|| {
                // None of the top few assembled; fall down the list for any route at all.
                ranking
                    .by_output_net_gas
                    .iter()
                    .skip(BASELINE_CANDIDATES)
                    .find_map(|&path_ix| build_baseline(path_ix))
            });

        // No early exit on a missing single path: a split across thin components can fill an order
        // that no single path can, so the caller decides — it only errors when neither a
        // single path nor a split candidate fills the order.
        input.ordered = ranking
            .by_output
            .iter()
            .map(|&path_ix| input.ordered[path_ix].clone())
            .collect();

        debug!(
            candidate_paths = input.ordered.len(),
            elapsed_ms = deadline.elapsed().as_millis(),
            "water-fill discovery + full-amount ranking"
        );
        Ok(SetupResult { input, best_single, cache })
    }

    /// Simulates every path at the full order amount and ranks them by what they pay.
    ///
    /// The paths overlap heavily — thousands open with the same pool, all carrying the whole order
    /// into it — so every hop goes through `cache` and each distinct swap costs one simulation no
    /// matter how many paths make it.
    ///
    /// A path that crosses one pool twice is dropped instead of ranked: its second crossing would
    /// have to see its own earlier swap, and every swap here reads untouched state.
    ///
    /// Nothing here builds a route. Only the baseline `setup` picks out of the ranking is worth
    /// copying a component and a pool state per leg.
    fn rank_at_full_amount<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        cache: &mut SwapCache<'g>,
    ) -> FullAmountRanking {
        let paths = &input.ordered;
        let order_amount = input.order.amount();
        let mut outcomes_by_path: Vec<Option<FullAmountOutcome>> = vec![None; paths.len()];

        for (path_ix, path) in paths.iter().enumerate() {
            if input.deadline.expired() {
                break;
            }
            if path_reuses_component(path) {
                continue;
            }
            outcomes_by_path[path_ix] = Some(
                match simulate_path(
                    path,
                    &input.market,
                    cache,
                    order_amount.clone(),
                    SolveStage::Ranking,
                ) {
                    Some(paid) => FullAmountOutcome::Filled(paid),
                    None => FullAmountOutcome::Unfilled,
                },
            );
        }

        rank_outcomes(
            outcomes_by_path,
            &input.gas_price,
            input.token_prices.as_ref(),
            input.order.token_out(),
        )
    }

    /// Every path the graph holds between the order's tokens, best spot-price-times-depth score
    /// first, cut to `max_candidates`. A path no edge weight covers sorts behind every scored one.
    fn top_scored_paths<'a>(
        &self,
        graph: &'a TopologyGraph<DepthAndPrice>,
        order: &Order,
    ) -> Result<Vec<Path<'a, DepthAndPrice>>, AlgorithmError> {
        let all_paths =
            paths::find_paths(graph, order.token_in(), order.token_out(), &self.query, None)?;
        if all_paths.is_empty() {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        let path_count = all_paths.len();
        let mut scored_count = 0usize;

        // A path no edge weight covers sorts behind every scored one rather than level with a
        // path scored at zero: the score is a spot price times a depth, so zero is a real score
        // that a weightless path has not earned.
        let mut scored: Vec<(Path<DepthAndPrice>, f64)> = all_paths
            .into_iter()
            .map(|path| {
                let score = match paths::try_score_path(&path) {
                    Some(score) => {
                        scored_count += 1;
                        score
                    }
                    None => f64::MIN,
                };
                (path, score)
            })
            .collect();
        scored.sort_by(|(_, a), (_, b)| {
            b.partial_cmp(a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        trace!(path_count, scored_count, limit = self.max_candidates, "water-fill path scoring");

        scored.truncate(self.max_candidates);

        Ok(scored
            .into_iter()
            .map(|(p, _)| p)
            .collect())
    }
}

impl Algorithm for WaterFillAlgorithm {
    type GraphType = TopologyGraph<DepthAndPrice>;
    type GraphManager = TopologyGraphManager<DepthAndPrice>;

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
        let deadline = Deadline::new(Instant::now(), self.timeout);
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let SetupResult { input, best_single, mut cache } = self
            .setup(graph, market, label, derived, order, deadline)
            .await?;

        // Build the split candidates; the best net of them competes with the single path.
        let mut candidates: Vec<SplitCandidate> = Vec::new();
        // One coarse (20-chunk) water-fill over the component-disjoint paths feeds both the floor
        // split and the refined split, so run it once. It is cheap and always finishes, so
        // a tight timeout cannot cut it off while leaving the single path — a winning split
        // is never lost to the clock.
        let disjoint = Self::select_disjoint(&input.ordered, self.max_paths);
        let coarse = (disjoint.len() >= 2)
            .then(|| self.disjoint_waterfill(&input, &disjoint, COARSE_CHUNKS, true))
            .flatten();
        if let Some(coarse) = coarse.as_deref() {
            // The 20-chunk floor split: exactly the coarse allocation.
            if let Some(c) = self.build_disjoint_legs(&input, &disjoint, coarse) {
                candidates.push(c);
            }
            // Finer allocation over the same active set (a bonus; a timeout may cut it off, and
            // then the floor stands).
            if let Some(c) =
                self.disjoint_refine(&input, &disjoint, coarse, FINE_CHUNKS, &mut cache)
            {
                candidates.push(c);
            }
        }
        // Fill-and-spill: a split that lets paths share a component and branch at an intermediate
        // token (a tree route), which the component-disjoint splits cannot express.
        if let Some(c) = self.fillspill_alloc(&input, FINE_CHUNKS, &mut cache) {
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
            let net = cand.net(&input);
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
        sim_meter::report("water_fill", &input.market, || deadline.elapsed().as_millis() as u64);
        debug!(
            candidate_count,
            split_won,
            elapsed_ms = deadline.elapsed().as_millis(),
            "water-fill selected {}",
            if split_won { "split candidate" } else { "single path" }
        );
        match best {
            Some((net, cand)) => Ok(RouteResult::new(cand.route, net, input.gas_price.clone())),
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
    fn disjoint_refine<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        disjoint: &[usize],
        coarse: &[BigUint],
        fine_chunks: usize,
        cache: &mut SwapCache<'g>,
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
        let fine = self.disjoint_waterfill(input, &active, fine_chunks, false)?;

        // Exchange refinement. The fine water-fill quantizes each path to a whole chunk, so it can
        // sit up to one chunk off the equal-marginal optimum. Nudge flow between paths at sub-chunk
        // resolution, accepting only strictly-improving moves (never-lose).
        let refined = self.disjoint_exchange(input, &active, fine_chunks, fine, cache);
        self.build_disjoint_legs(input, &active, &refined)
    }

    /// Net output (gross output minus gas cost in output-token terms) of `path` simulated in
    /// isolation at `amount`. Component-disjoint paths never interfere, so an isolated
    /// re-simulation is exact. A zero amount means the path is dropped from the route: it
    /// yields no output and, since it is no longer swapped, no gas — so dropping a donor
    /// credits its saved gas automatically.
    fn path_net<'g>(
        input: &SolveInput<'_, 'g>,
        path: &Path<'g, DepthAndPrice>,
        amount: &BigUint,
        cache: &mut SwapCache<'g>,
    ) -> Option<BigInt> {
        if amount.is_zero() {
            return Some(BigInt::zero());
        }
        let result =
            simulate_path(path, &input.market, cache, amount.clone(), SolveStage::Exchange)?;
        let activation = Self::activation_cost(input, &result.gas);
        Some(BigInt::from(result.amount_out) - activation)
    }

    /// Exchange-refinement pass over the fixed active set, starting from the fine water-fill split.
    /// Water-fill can never un-commit a chunk, so its split is only accurate to one fine chunk and
    /// can miss the equal-marginal split. This shifts `delta` of input from an over-allocated donor
    /// to an under-allocated recipient whenever the pair's summed net output strictly improves,
    /// then halves `delta` once no move helps, down to a sub-chunk floor. Paths are
    /// component-disjoint, so a trial re-simulates only the two paths it touches (unchanged
    /// paths keep their cached net). Only strictly-improving moves are accepted, so the result
    /// never scores below the split it started from.
    fn disjoint_exchange<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        active: &[usize],
        fine_chunks: usize,
        alloc: Vec<BigUint>,
        cache: &mut SwapCache<'g>,
    ) -> Vec<BigUint> {
        let path_count = active.len();
        if path_count < 2 {
            return alloc;
        }
        let amount_in = input.order.amount().clone();
        let fine_chunks = fine_chunks.max(1);
        let mut delta = &amount_in / fine_chunks;
        let min_delta = &amount_in / (fine_chunks * EXCHANGE_DELTA_FLOOR);
        if delta.is_zero() {
            return alloc;
        }

        let mut cumulative_amount_in = alloc;
        // Cache each active path's net at its current cumulative_amount_in amount so a pair trial
        // only re-simulates the two paths it moves flow between, not the whole active set.
        let mut net_cache: Vec<BigInt> = Vec::with_capacity(path_count);
        for (i, &path_idx) in active.iter().enumerate() {
            let Some(net) =
                Self::path_net(input, &input.ordered[path_idx], &cumulative_amount_in[i], cache)
            else {
                // The starting split does not simulate cleanly; refining it is unsafe, so keep it.
                return cumulative_amount_in;
            };
            net_cache.push(net);
        }

        let mut sims = 0usize;
        while delta >= min_delta && !delta.is_zero() {
            if input.deadline.expired() || sims >= EXCHANGE_MAX_SIMS {
                break;
            }

            let mut best: Option<ExchangeMove> = None;
            for donor in 0..path_count {
                if sims >= EXCHANGE_MAX_SIMS {
                    break;
                }
                if cumulative_amount_in[donor] < delta {
                    continue;
                }
                let donor_amt = &cumulative_amount_in[donor] - &delta;
                let Some(donor_net) =
                    Self::path_net(input, &input.ordered[active[donor]], &donor_amt, cache)
                else {
                    continue;
                };
                sims += 1;
                for recipient in 0..path_count {
                    if sims >= EXCHANGE_MAX_SIMS {
                        break;
                    }
                    if recipient == donor {
                        continue;
                    }
                    let recip_amt = &cumulative_amount_in[recipient] + &delta;
                    let Some(recip_net) =
                        Self::path_net(input, &input.ordered[active[recipient]], &recip_amt, cache)
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
            cumulative_amount_in[mv.donor] = &cumulative_amount_in[mv.donor] - &delta;
            cumulative_amount_in[mv.recipient] = &cumulative_amount_in[mv.recipient] + &delta;
            net_cache[mv.donor] = mv.donor_net;
            net_cache[mv.recipient] = mv.recip_net;
        }
        cumulative_amount_in
    }

    /// Simulates `amount` through `path`, reading and committing component states via `overrides`,
    /// and returns the allocation the route assembly consumes.
    fn allocation_commit<'g>(
        path: &Path<'g, DepthAndPrice>,
        market: &MarketState,
        overrides: &mut MarketOverrides,
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
            let state = hop_state(market, component_id, None, Some(overrides))?;
            let result = state
                .get_amount_out_metered(
                    component_id,
                    SolveStage::Assembly.label(),
                    current.clone(),
                    token_in,
                    token_out,
                )
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
        input: &SolveInput,
        allocations: &[PathAllocation],
    ) -> Option<SplitCandidate> {
        let route = build_split_route(allocations, &input.market, input.order).ok()?;
        let token_out = input.order.token_out();
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
    /// aligned by index; component-disjoint paths never interfere, so each leg is a real
    /// independent simulation.
    fn build_disjoint_legs<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        subset: &[usize],
        alloc: &[BigUint],
    ) -> Option<SplitCandidate> {
        let amount_in = input.order.amount().clone();
        let mut allocations = Vec::new();
        for (i, &path_idx) in subset.iter().enumerate() {
            if alloc[i].is_zero() {
                continue;
            }
            // Fresh overrides per leg: legs are component-disjoint, but a component reused within
            // one path must still see its own first swap.
            let mut overrides = MarketOverrides::empty();
            let allocation = Self::allocation_commit(
                &input.ordered[path_idx],
                &input.market,
                &mut overrides,
                alloc[i].clone(),
                ratio(&alloc[i], &amount_in),
            )?;
            allocations.push(allocation);
        }
        if allocations.is_empty() {
            return None;
        }
        Self::candidate_from_allocations(input, &allocations)
    }

    /// Incremental water-fill over a set of component-disjoint paths. Returns the amount allocated
    /// to each path in `subset` order. With `gate`, a path only activates when its first chunk
    /// covers its gas; without it, every path is eligible (used once the active set is fixed).
    fn disjoint_waterfill<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        subset: &[usize],
        num_chunks: usize,
        gate: bool,
    ) -> Option<Vec<BigUint>> {
        let amount_in = input.order.amount().clone();
        let num_chunks = num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            return None;
        }
        let remainder = &amount_in - &base_chunk * num_chunks;
        let path_count = subset.len();

        let mut committed: Vec<MarketOverrides> = (0..path_count)
            .map(|_| MarketOverrides::empty())
            .collect();
        let mut cumulative_amount_in: Vec<BigUint> = vec![BigUint::zero(); path_count];
        let mut activated: Vec<bool> = vec![!gate; path_count];

        // What each path last paid for a chunk. Only the path that wins a chunk commits anything,
        // and these paths share no component, so every other path is asked the same question of the
        // same untouched pools next chunk and pays the same. Its marginal is kept rather than
        // simulated again.
        let mut marginals: Vec<Option<StepResult>> = (0..path_count).map(|_| None).collect();
        // The chunk every remembered marginal was priced at. The first one carries the remainder,
        // so what the paths are asked changes once and nothing remembered still answers it.
        let mut marginals_chunk: Option<BigUint> = None;

        for chunk_idx in 0..num_chunks {
            if input.deadline.expired() {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };
            if marginals_chunk.as_ref() != Some(&chunk) {
                marginals
                    .iter_mut()
                    .for_each(|m| *m = None);
                marginals_chunk = Some(chunk.clone());
            }

            let mut best: Option<(usize, BigInt)> = None;
            for (i, &path_idx) in subset.iter().enumerate() {
                if marginals[i].is_none() {
                    marginals[i] = Self::simulate_step(
                        &input.ordered[path_idx],
                        &input.market,
                        &committed[i],
                        chunk.clone(),
                    );
                }
                let Some(step) = marginals[i].as_ref() else {
                    continue;
                };
                let gross_marginal = BigInt::from(step.amount_out.clone());
                let net_marginal = if activated[i] {
                    gross_marginal
                } else {
                    let activation = Self::activation_cost(input, &step.gas);
                    gross_marginal - activation
                };
                if best
                    .as_ref()
                    .map(|(_, m)| &net_marginal > m)
                    .unwrap_or(true)
                {
                    best = Some((i, net_marginal));
                }
            }

            let Some((best_i, _)) = best else {
                break;
            };
            // Take the winner's marginal rather than read it: its pools are about to move, so
            // what it just paid no longer answers, and taking it is how that is forgotten.
            let Some(step) = marginals[best_i].take() else {
                break;
            };

            for (id, state) in step.new_states {
                committed[best_i].insert(id, state);
            }
            cumulative_amount_in[best_i] += &chunk;
            activated[best_i] = true;
        }
        Some(cumulative_amount_in)
    }

    /// Selects fill-and-spill candidates: the top full-amount paths plus the best first-chunk
    /// marginal probes. The probe is what makes intermediate-token splits (tree routes) reachable:
    /// the extra path often ranks poorly at full size but wins on the margin.
    fn select_shared_candidates<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        cache: &mut SwapCache<'g>,
    ) -> Vec<usize> {
        let mut candidates: Vec<usize> = (0..input
            .ordered
            .len()
            .min(SHARED_FULL_PATHS))
            .collect();
        let first_chunk = input.order.amount() / COARSE_CHUNKS;
        if first_chunk.is_zero() {
            return candidates;
        }
        let mut marginal: Vec<(usize, BigInt)> = Vec::new();
        for (idx, path) in input
            .ordered
            .iter()
            .enumerate()
            .take(SHARED_MARGIN_PROBE_PATHS)
        {
            if input.deadline.expired() {
                break;
            }
            // Nothing is committed yet, so these probes read untouched state and go through the
            // cache like every other swap that does.
            let Some(probe) = simulate_path(
                path,
                &input.market,
                cache,
                first_chunk.clone(),
                SolveStage::SetSelection,
            ) else {
                continue;
            };
            let activation = Self::activation_cost(input, &probe.gas);
            marginal.push((idx, BigInt::from(probe.amount_out) - activation));
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

    /// Coarse set-selection then fine allocation with shared-component fill-and-spill.
    ///
    /// Only the probes that pick the candidate set can use `cache`: the two water-fill passes below
    /// commit each chunk into an overlay and must see the pools they have already drained.
    fn fillspill_alloc<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        fine_chunks: usize,
        cache: &mut SwapCache<'g>,
    ) -> Option<SplitCandidate> {
        let candidates = self.select_shared_candidates(input, cache);
        if candidates.len() < 2 {
            return None;
        }

        // Phase 1: coarse gated pass to choose the active candidate set — the candidates that
        // took at least one chunk.
        let coarse = self.fillspill_waterfill(input, &candidates, COARSE_CHUNKS, true)?;
        let took_a_chunk: FxHashSet<usize> = coarse.iter().map(|(i, _)| *i).collect();
        let active: Vec<usize> = candidates
            .iter()
            .copied()
            .enumerate()
            .filter(|(i, _)| took_a_chunk.contains(i))
            .map(|(_, idx)| idx)
            .collect();
        if active.len() < 2 {
            return None;
        }

        // Phase 2: fine ungated pass over the active set, with the commit schedule for replay.
        let schedule = self.fillspill_waterfill(input, &active, fine_chunks, false)?;
        if schedule.is_empty() {
            return None;
        }

        self.build_fillspill_route(input, &active, &schedule)
    }

    /// Incremental fill-and-spill water-fill over a single shared overlay. Returns the ordered
    /// commit schedule of `(subset_index, chunk_amount)`; which candidates took a chunk at all is
    /// read off it.
    fn fillspill_waterfill<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        subset: &[usize],
        num_chunks: usize,
        gate: bool,
    ) -> Option<Vec<(usize, BigUint)>> {
        let amount_in = input.order.amount().clone();
        let num_chunks = num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            return None;
        }
        let remainder = &amount_in - &base_chunk * num_chunks;

        let mut overlay = MarketOverrides::empty();
        let mut activated: Vec<bool> = vec![!gate; subset.len()];
        let mut active_count = if gate { 0 } else { subset.len() };
        let mut schedule: Vec<(usize, BigUint)> = Vec::with_capacity(num_chunks);
        // What each candidate last paid for a chunk, kept so a candidate the winning chunk did not
        // touch is not asked the same question again. Candidates here may share pools — that is the
        // point of fill-and-spill — so committing a chunk forgets every candidate crossing one of
        // the pools it moved, not only the winner.
        let mut marginals: Vec<Option<StepResult>> = (0..subset.len())
            .map(|_| None)
            .collect();
        // The chunk every remembered marginal was priced at. The first one carries the remainder,
        // so what the candidates are asked changes once and nothing remembered still answers it.
        let mut marginals_chunk: Option<BigUint> = None;

        for chunk_idx in 0..num_chunks {
            if input.deadline.expired() {
                break;
            }
            let chunk = if chunk_idx == 0 { &base_chunk + &remainder } else { base_chunk.clone() };
            if marginals_chunk.as_ref() != Some(&chunk) {
                marginals
                    .iter_mut()
                    .for_each(|m| *m = None);
                marginals_chunk = Some(chunk.clone());
            }

            let mut best: Option<(usize, BigInt)> = None;
            for (i, &path_idx) in subset.iter().enumerate() {
                if !activated[i] && active_count >= self.max_paths {
                    continue;
                }
                if marginals[i].is_none() {
                    marginals[i] = Self::simulate_step(
                        &input.ordered[path_idx],
                        &input.market,
                        &overlay,
                        chunk.clone(),
                    );
                }
                let Some(step) = marginals[i].as_ref() else {
                    continue;
                };
                let gross_marginal = BigInt::from(step.amount_out.clone());
                let net_marginal = if activated[i] {
                    gross_marginal
                } else {
                    let activation = Self::activation_cost(input, &step.gas);
                    gross_marginal - activation
                };
                if best
                    .as_ref()
                    .map(|(_, m)| &net_marginal > m)
                    .unwrap_or(true)
                {
                    best = Some((i, net_marginal));
                }
            }

            let Some((best_i, _)) = best else {
                break;
            };
            // Take the winner's marginal rather than read it: taking is how it is forgotten.
            // `best` is only set while that entry holds a marginal, so the `else` cannot be
            // reached; it stands in for an unwrap the crate's lints forbid.
            let Some(step) = marginals[best_i].take() else {
                break;
            };

            let pools_moved: FxHashSet<&ComponentId> = step
                .new_states
                .iter()
                .map(|(id, _)| id)
                .collect();
            for (i, &path_idx) in subset.iter().enumerate() {
                if input.ordered[path_idx]
                    .edge_iter()
                    .iter()
                    .any(|e| pools_moved.contains(&e.component_id))
                {
                    marginals[i] = None;
                }
            }

            for (id, state) in step.new_states {
                overlay.insert(id, state);
            }
            if !activated[best_i] {
                activated[best_i] = true;
                active_count += 1;
            }
            schedule.push((best_i, chunk));
        }

        Some(schedule)
    }

    /// Rebuilds the fill-and-spill result as one leg per active path at its total allocated
    /// amount, committed sequentially (largest allocation first) against a shared overlay — the
    /// same execution model the router applies on-chain.
    fn build_fillspill_route<'g>(
        &self,
        input: &SolveInput<'_, 'g>,
        active: &[usize],
        schedule: &[(usize, BigUint)],
    ) -> Option<SplitCandidate> {
        let amount_in = input.order.amount().clone();
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

        let mut overrides = MarketOverrides::empty();
        let mut allocations = Vec::new();
        for i in execution_order {
            let allocation = Self::allocation_commit(
                &input.ordered[active[i]],
                &input.market,
                &mut overrides,
                cand_in[i].clone(),
                ratio(&cand_in[i], &amount_in),
            )?;
            allocations.push(allocation);
        }
        Self::candidate_from_allocations(input, &allocations)
    }
}

/// Whether `path` crosses the same component more than once. Such a path has to see its own
/// earlier swap, so it cannot be simulated against untouched component state.
fn path_reuses_component<W>(path: &Path<'_, W>) -> bool {
    let mut seen: FxHashSet<&ComponentId> =
        FxHashSet::with_capacity_and_hasher(path.len(), Default::default());
    !path
        .edge_iter()
        .iter()
        .all(|e| seen.insert(&e.component_id))
}

/// Swaps `amount_in` through one hop against the market's untouched state. `None` when the market
/// holds no token or state for the hop, or the pool refuses the swap.
fn simulate_hop(
    market: &MarketState,
    component_id: &ComponentId,
    address_in: &Address,
    address_out: &Address,
    amount_in: &BigUint,
    pass: SolveStage,
) -> Result<SwapResult, Refusal> {
    let (Some(token_in), Some(token_out), Some(state)) = (
        market.get_token(address_in),
        market.get_token(address_out),
        market.get_simulation_state(component_id),
    ) else {
        return Err(Refusal::Failed);
    };
    state
        .get_amount_out_metered(component_id, pass.label(), amount_in.clone(), token_in, token_out)
        .map(|result| SwapResult { amount_out: result.amount, gas: result.gas })
        .map_err(|error| Refusal::of(&error))
}

/// The state a hop swaps against: what this path has already moved, then what the pass has
/// committed, then the market as it stands.
///
/// The precedence lives here rather than in each pass's own loop, so two passes cannot quietly
/// disagree about which state a hop sees. `intra_path` is `None` for a pass whose paths never
/// cross a pool twice, and `committed` is `None` for one that reads untouched state.
fn hop_state<'s>(
    market: &'s MarketState,
    component_id: &ComponentId,
    intra_path: Option<&'s FxHashMap<ComponentId, Box<dyn ProtocolSim>>>,
    committed: Option<&'s MarketOverrides>,
) -> Option<&'s dyn ProtocolSim> {
    intra_path
        .and_then(|states| {
            states
                .get(component_id)
                .map(Box::as_ref)
        })
        .or_else(|| committed.and_then(|overrides| overrides.get(component_id)))
        .or_else(|| market.get_simulation_state(component_id))
}

/// Swaps `amount_in` along `path` against untouched component state, going through `cache` so a
/// hop some other path already made is not made again. Returns what the path pays and its summed
/// gas, or `None` as soon as one hop refuses.
///
/// The pass must not hand this a path that crosses one pool twice: the second crossing would
/// have to see the first one's swap, and every hop here reads untouched state. Both path sets this
/// runs on are already free of them — `rank_at_full_amount` drops them, and discovery never builds
/// one because `can_extend_path` refuses to repeat a component.
fn simulate_path<'a>(
    path: &Path<'a, DepthAndPrice>,
    market: &MarketState,
    cache: &mut SwapCache<'a>,
    amount_in: BigUint,
    stage: SolveStage,
) -> Option<SwapResult> {
    // What the next hop swaps; once the last one is done, what the path pays out.
    let mut hop_amount_in = amount_in;
    let mut path_gas = BigUint::zero();

    for (address_in, edge, address_out) in path.iter() {
        let direction = PoolDirection { component_id: &edge.component_id, address_in, address_out };
        let hop = cache.swap(
            direction,
            &hop_amount_in,
            stage.label(),
            || {
                simulate_hop(
                    market,
                    &edge.component_id,
                    address_in,
                    address_out,
                    &hop_amount_in,
                    stage,
                )
            },
            stage.may_interpolate(),
        )?;
        hop_amount_in = hop.amount_out;
        path_gas += hop.gas;
    }

    Some(SwapResult { amount_out: hop_amount_in, gas: path_gas })
}

/// Turns the full-amount pass's per-path outcomes into the two orderings `setup` consumes. Both
/// are built in path order and sorted stably, so paths paying the same amount keep the order they
/// were enumerated in. A path with no outcome never ran — ranking timed out before reaching it,
/// or it crossed a pool twice — and appears in neither ordering.
fn rank_outcomes(
    outcomes_by_path: Vec<Option<FullAmountOutcome>>,
    gas_price: &BigUint,
    token_prices: Option<&TokenGasPrices>,
    token_out: &Address,
) -> FullAmountRanking {
    // A path that took the whole order carries what it pays net of gas; one that could not takes
    // no place among them, whatever its gas would have been.
    let mut by_output: Vec<(usize, Option<BigInt>)> = Vec::with_capacity(outcomes_by_path.len());
    let mut by_output_net_gas: Vec<(usize, BigInt)> = Vec::new();

    for (path_ix, outcome) in outcomes_by_path.into_iter().enumerate() {
        let Some(outcome) = outcome else {
            continue;
        };
        let net_output = match outcome {
            FullAmountOutcome::Filled(paid) => {
                let gas_cost = WaterFillAlgorithm::gas_cost_in_token(
                    &paid.gas,
                    gas_price,
                    token_prices,
                    token_out,
                );
                let net_output = BigInt::from(paid.amount_out.clone()) -
                    gas_cost.map_or_else(BigInt::zero, BigInt::from);
                by_output_net_gas.push((path_ix, net_output.clone()));
                Some(net_output)
            }
            FullAmountOutcome::Unfilled => None,
        };
        by_output.push((path_ix, net_output));
    }

    // A path that could not take the whole order ranks below every path that did, however little
    // that one is left with: net output can be negative once gas costs more than the path pays,
    // and a path that filled for a loss still tells the split passes more than one that failed.
    by_output.sort_by(|(_, a), (_, b)| b.cmp(a));
    by_output_net_gas.sort_by(|(_, a), (_, b)| b.cmp(a));
    FullAmountRanking {
        by_output: by_output
            .into_iter()
            .map(|(path_ix, _)| path_ix)
            .collect(),
        by_output_net_gas: by_output_net_gas
            .into_iter()
            .map(|(path_ix, _)| path_ix)
            .collect(),
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

/// Runs the bounded discovery and returns the candidate paths plus their `(index, full-amount
/// gross output)` ranking, best first.
fn discover_paths<'a, W>(
    graph: &'a TopologyGraph<W>,
    market: &MarketDataView<'_>,
    order: &Order,
    cache: &mut SwapCache<'a>,
    cfg: CandidateSearchConfig<'_>,
) -> Result<Vec<Path<'a, W>>, AlgorithmError>
where
    W: Clone,
{
    let (from_idx, to_idx) = get_token_ixs(graph, order)?;

    let mut found = Vec::new();
    let mut frontier = vec![CandidatePathState {
        node: from_idx,
        path: Path::new(),
        amount_out: order.amount().clone(),
    }];

    for _depth in 0..cfg.query.max_hops {
        if cfg.deadline.expired() || frontier.is_empty() {
            break;
        }
        let mut next_by_node: FxHashMap<NodeIndex, Vec<CandidatePathState<'a, W>>> =
            FxHashMap::default();
        for state in frontier {
            if state.node == to_idx && from_idx != to_idx {
                continue;
            }

            let mut discovery = Discovery { graph, market, cfg: &cfg, cache };
            expand_candidate_state(&mut discovery, to_idx, state, &mut found, &mut next_by_node);
        }
        frontier = prune_candidate_frontier(next_by_node);
    }

    rank_found_candidate_paths(found, cfg.max_candidates, order)
}

/// The graph nodes holding the order's sell and buy tokens.
///
/// # Errors
///
/// [`AlgorithmError::NoPath`] naming whichever of the two the graph does not hold.
fn get_token_ixs<W>(
    graph: &TopologyGraph<W>,
    order: &Order,
) -> Result<(NodeIndex, NodeIndex), AlgorithmError> {
    let missing = |reason| AlgorithmError::NoPath {
        from: order.token_in().clone(),
        to: order.token_out().clone(),
        reason,
    };
    let from_idx = graph
        .get_token_ix(order.token_in())
        .ok_or_else(|| missing(NoPathReason::SourceTokenNotInGraph))?;
    let to_idx = graph
        .get_token_ix(order.token_out())
        .ok_or_else(|| missing(NoPathReason::DestinationTokenNotInGraph))?;
    Ok((from_idx, to_idx))
}

fn expand_candidate_state<'a, W>(
    discovery: &mut Discovery<'a, '_, W>,
    target: NodeIndex,
    state: CandidatePathState<'a, W>,
    found: &mut Vec<(Path<'a, W>, BigUint)>,
    next_by_node: &mut FxHashMap<NodeIndex, Vec<CandidatePathState<'a, W>>>,
) where
    W: Clone,
{
    let cfg = discovery.cfg;
    let graph = discovery.graph;
    let edges = candidate_edges_for_state(discovery, target, &state);
    for candidate in edges {
        if cfg.deadline.expired() {
            break;
        }
        let mut path = state.path.clone();
        path.add_hop(&graph[state.node], candidate.edge, &graph[candidate.target]);
        let path_state = CandidatePathState {
            node: candidate.target,
            path: path.clone(),
            amount_out: candidate.amount_out,
        };
        if candidate.target == target && path.len() >= cfg.query.min_hops {
            found.push((path.clone(), path_state.amount_out.clone()));
        }
        if path.len() < cfg.query.max_hops {
            next_by_node
                .entry(candidate.target)
                .or_default()
                .push(path_state);
        }
    }
}

fn candidate_edges_for_state<'a, W>(
    discovery: &mut Discovery<'a, '_, W>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
) -> Vec<ScoredEdge<'a, W>> {
    let mut preferred = score_candidate_edges(discovery, target, state, true);
    if preferred.is_empty() {
        preferred = score_candidate_edges(discovery, target, state, false);
    }
    select_candidate_edges(preferred, CANDIDATE_EDGES_PER_STATE)
}

fn score_candidate_edges<'a, W>(
    discovery: &mut Discovery<'a, '_, W>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
    preferred_only: bool,
) -> Vec<ScoredEdge<'a, W>> {
    let Discovery { graph, market, cfg, cache } = discovery;
    let graph = *graph;
    let market = *market;
    let cfg = *cfg;
    let mut scored = Vec::new();
    for edge in graph.edges(state.node) {
        let next_node = edge.target();
        // Priority is a property of the token reached, so it is settled before looking at the pools
        // that reach it.
        let priority = match candidate_priority(graph, next_node, target, cfg) {
            Some(priority) => priority,
            None if preferred_only => continue,
            None => 3,
        };
        for pool in edge.weight().pools() {
            if !can_extend_path(graph, state, next_node, target, pool, cfg) {
                continue;
            }
            let address_in = &graph[state.node];
            let address_out = &graph[next_node];
            let direction =
                PoolDirection { component_id: &pool.component_id, address_in, address_out };
            let Some(hop) = cache.swap(
                direction,
                &state.amount_out,
                SolveStage::Discovery.label(),
                || simulate_edge(market, &state.amount_out, address_in, pool, address_out),
                SolveStage::Discovery.may_interpolate(),
            ) else {
                continue;
            };
            scored.push(ScoredEdge {
                target: next_node,
                edge: pool,
                amount_out: hop.amount_out,
                priority,
            });
        }
    }
    scored
}

/// Derives bounded discovery's soft anchor set from the live graph: the `DERIVED_ANCHOR_COUNT`
/// most-connected tokens (most pools trading them) plus the native-ETH sentinel. Degree is
/// the same connectivity signal `derive-connector-tokens` ranks by, so anchoring stays correct on
/// any chain without a hardcoded per-chain list. The native-ETH zero address carries near-zero
/// degree but is load-bearing for `WETH → ETH → token` routes where Tycho models native ETH as
/// `0x0`, so it is anchored explicitly.
///
/// Pools, not trading partners: this graph holds one edge per token pair with the pools inside it,
/// so counting edges would rank a token on five thin pairs above one on three deep ones.
fn derive_anchor_tokens<W>(graph: &TopologyGraph<W>) -> FxHashSet<Address> {
    let mut by_pool_count: Vec<(NodeIndex, usize)> = graph
        .node_indices()
        .map(|node| {
            let pools = graph
                .edges(node)
                .map(|edge| edge.weight().pools().len())
                .sum();
            (node, pools)
        })
        .collect();
    by_pool_count.sort_unstable_by_key(|(_, pools)| Reverse(*pools));
    let mut anchors: FxHashSet<Address> = by_pool_count
        .into_iter()
        .take(DERIVED_ANCHOR_COUNT)
        .map(|(node, _)| graph[node].clone())
        .collect();
    anchors.insert(Address::from([0u8; 20]));
    anchors
}

fn candidate_priority<W>(
    graph: &TopologyGraph<W>,
    node: NodeIndex,
    target: NodeIndex,
    cfg: &CandidateSearchConfig<'_>,
) -> Option<u8> {
    if node == target {
        return Some(0);
    }
    let token = &graph[node];
    match cfg.query.connector_tokens.as_ref() {
        Some(tokens) => tokens.contains(token).then_some(1),
        None => cfg
            .anchor_tokens
            .contains(token)
            .then_some(2),
    }
}

fn can_extend_path<W>(
    graph: &TopologyGraph<W>,
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
    cfg.query
        .connector_tokens
        .as_ref()
        .map(|tokens| tokens.contains(next_addr))
        .unwrap_or(true)
}

fn simulate_edge<W>(
    market: &MarketDataView<'_>,
    amount: &BigUint,
    token_in_addr: &Address,
    edge: &EdgeData<W>,
    token_out_addr: &Address,
) -> Result<SwapResult, Refusal> {
    let (Some(token_in), Some(token_out), Some(state)) = (
        market.get_token(token_in_addr),
        market.get_token(token_out_addr),
        market.get_simulation_state(&edge.component_id),
    ) else {
        return Err(Refusal::Failed);
    };
    state
        .get_amount_out_metered(
            &edge.component_id,
            SolveStage::Discovery.label(),
            amount.clone(),
            token_in,
            token_out,
        )
        .map(|result| SwapResult { amount_out: result.amount, gas: result.gas })
        .map_err(|error| Refusal::of(&error))
}

fn select_candidate_edges<W>(
    mut scored: Vec<ScoredEdge<'_, W>>,
    max_edges: usize,
) -> Vec<ScoredEdge<'_, W>> {
    scored.sort_by(compare_scored_edges);
    let mut selected = Vec::new();
    let mut per_target: FxHashMap<NodeIndex, usize> = FxHashMap::default();
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
    by_node: FxHashMap<NodeIndex, Vec<CandidatePathState<'_, W>>>,
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
) -> Result<Vec<Path<'a, W>>, AlgorithmError> {
    found.sort_by(|(_, a), (_, b)| b.cmp(a));
    let mut keys = FxHashSet::default();
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
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use num_traits::ToPrimitive;
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

    use super::{super::MostLiquidAlgorithm, *};
    use crate::{
        algorithm::{
            split_test_harness::{
                evaluate_scenario, optimal_two_component_output, split_metrics, split_scenarios,
                two_equal_weth_usdc, TWO_EQUAL_USDC_RESERVE, TWO_EQUAL_WETH_RESERVE,
            },
            test_utils::{
                addr, setup_market_unweighted_topology, setup_market_weighted_boxed,
                token_with_decimals, ConstantProductSim, DivByZeroSim,
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

    fn v2_component(reserve_a: u128, reserve_b: u128) -> UniswapV2State {
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
            // scenarios. DOUBLE_SPLIT re-splits at both hops with mismatched component sizes; the
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

    /// A split fills an order that no single path can. Two parallel constant-product components
    /// each revert once the input reaches their reserve, so the full order overruns either
    /// component alone; split in half it fits both. The portfolio returns the split even though
    /// the single-path baseline is absent — instead of the earlier `InsufficientLiquidity`
    /// short-circuit.
    #[tokio::test]
    async fn test_split_fills_when_no_single_path_can() {
        let a = token_with_decimals(0x01, "A", 18);
        let b = token_with_decimals(0x02, "B", 18);
        // reserve_0 (token A, the input side) = 800; a swap reverts once its input reaches it. The
        // full order (1000 A) overruns either component, but ~500 A per component in a 50/50 split
        // fits.
        let component = || {
            Box::new(ConstantProductSim {
                reserve_0: BigUint::from(800u64) * BigUint::from(10u64).pow(18),
                reserve_1: BigUint::from(10_000u64) * BigUint::from(10u64).pow(18),
                gas: 50_000,
            }) as Box<dyn ProtocolSim>
        };
        let (market, gm) = setup_market_weighted_boxed(vec![
            ("component_x", &a, &b, component()),
            ("component_y", &a, &b, component()),
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

        // The portfolio still returns a split across both components.
        let split = WaterFillAlgorithm::with_config(config())
            .unwrap()
            .find_best_route(gm.graph(), market.clone(), None, None, &order)
            .await
            .expect("water_fill returns a split when no single path fills");
        assert!(split.route().swaps().len() >= 2, "expected a split across both components");
    }

    /// A component whose math panics mid-simulation must be skipped like any failing component,
    /// not unwind through the solver worker thread. The panicking component advertises huge depth
    /// so discovery ranks it first and the bulk fill loops actually simulate it.
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
            ("component_ok", &a, &b, healthy),
            ("component_panics", &a, &b, Box::new(DivByZeroSim::default())),
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
            .expect("panicking component is skipped; the healthy component still fills the order");

        assert!(
            result
                .route()
                .swaps()
                .iter()
                .all(|swap| swap.component_id() == "component_ok"),
            "route must only use the healthy component",
        );
    }

    /// On two equal fee-free components the split's gross output must come within a tight tolerance
    /// of the analytical two-component optimum (a 50/50 split): the fine 256-chunk allocation
    /// finds the optimal split, not just any splitting one.
    #[tokio::test]
    async fn test_water_fill_output_near_two_component_optimum() {
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
        assert_eq!(path_count, 2, "the optimum uses both components");

        // Fee-free reserves in raw units, so the no-fee two-component optimum applies directly.
        let reserve_in = TWO_EQUAL_WETH_RESERVE as f64 * 1e18;
        let reserve_out = TWO_EQUAL_USDC_RESERVE as f64 * 1e6;
        let trade_amount = trade as f64 * 1e18;
        let (_, optimum) = optimal_two_component_output(
            reserve_in,
            reserve_out,
            reserve_in,
            reserve_out,
            trade_amount,
        );

        let gross = gross.to_f64().unwrap();
        assert!(
            gross >= optimum * 0.999 && gross <= optimum * 1.0001,
            "split gross {gross} should be within 0.1% of the two-component optimum {optimum}",
        );
    }

    /// Under a tight timeout the split must never lose to the best single path: the 20-chunk split
    /// does the cheap coarse allocation, so cutting off later refinement cannot drop the result
    /// below the single path. A budget too tight to finish yields no route at all (the router falls
    /// back to other components) — a no-answer, not a loss — so the never-lose check runs only for
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

    /// Bounded discovery finds both parallel components as candidate paths and ranks the deeper
    /// component first by simulated full-amount output, using live simulation only (no
    /// precomputed edge weights on the weightless graph).
    #[tokio::test]
    async fn test_discovery_finds_and_ranks_parallel_components() {
        let link = token_with_decimals(0x01, "LINK", 18);
        let weth = token_with_decimals(0x02, "WETH", 18);
        let (market, graph_manager) = setup_market_unweighted_topology(vec![
            (
                "a_weak_link_weth",
                &link,
                &weth,
                Box::new(v2_component(2_000_000, 264)) as Box<dyn ProtocolSim>,
            ),
            (
                "z_strong_link_weth",
                &link,
                &weth,
                Box::new(v2_component(2_000_000, 5_700)) as Box<dyn ProtocolSim>,
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
        let paths = discover_paths(
            graph_manager.graph(),
            &view,
            &order,
            &mut SwapCache::new(),
            CandidateSearchConfig {
                query: &GraphQueryFilter { min_hops: 1, max_hops: 3, connector_tokens: None },
                max_candidates: 128,
                anchor_tokens: &FxHashSet::default(),
                source_token: order.token_in(),
                deadline: Deadline::new(start, Duration::from_millis(2000)),
            },
        )
        .expect("discovery finds candidates");

        assert_eq!(paths.len(), 2, "both parallel components should be discovered");
        // Scores are (path index, full-amount gross output), best first: the deeper component wins.
        let best_path = &paths[0];
        assert_eq!(
            best_path.edge_iter()[0].component_id,
            "z_strong_link_weth",
            "discovery should rank by simulated output, not topology or edge weights",
        );
    }

    // ==================== Ranking the full-amount outcomes ====================

    fn hop(amount_out: u64, gas: u64) -> SwapResult {
        SwapResult { amount_out: BigUint::from(amount_out), gas: BigUint::from(gas) }
    }

    /// `Unfilled` paths rank last by output and are kept out of the net-of-gas ordering entirely,
    /// so one can never become the single-path baseline. A path with no outcome is in neither.
    #[test]
    fn test_rank_outcomes_places_unfilled_and_missing_paths() {
        let outcomes = vec![
            Some(FullAmountOutcome::Filled(hop(1000, 0))),
            Some(FullAmountOutcome::Unfilled),
            None,
            Some(FullAmountOutcome::Filled(hop(3000, 0))),
        ];

        let ranking = rank_outcomes(outcomes, &BigUint::from(1u64), None, &addr(0x02));

        assert_eq!(ranking.by_output, vec![3, 0, 1], "unfilled ranks last, missing is absent");
        assert_eq!(ranking.by_output_net_gas, vec![3, 0], "unfilled cannot be the baseline");
    }

    /// Both orderings take gas off, so a path paying more gross ranks below a cheaper one in each.
    /// The candidate set the split passes are built from is sliced off `by_output`, and ranking
    /// that gross put a long route above a short one on an output its extra swaps hand back.
    #[test]
    fn test_rank_outcomes_orders_by_output_net_of_gas() {
        let token_out = addr(0x02);
        let mut token_prices = TokenGasPrices::default();
        token_prices.insert(
            token_out.clone(),
            tycho_simulation::tycho_common::simulation::protocol_sim::Price::new(
                BigUint::from(1u64),
                BigUint::from(1u64),
            ),
        );
        let outcomes = vec![
            Some(FullAmountOutcome::Filled(hop(1000, 500))),
            Some(FullAmountOutcome::Filled(hop(900, 10))),
        ];

        let ranking =
            rank_outcomes(outcomes, &BigUint::from(1u64), Some(&token_prices), &token_out);

        assert_eq!(ranking.by_output, vec![1, 0], "the cheaper path ranks first");
        assert_eq!(ranking.by_output_net_gas, vec![1, 0], "and the baseline ordering agrees");
    }

    /// A path that filled but paid less than its gas ranks below every profitable path and above
    /// every path that could not fill: net output goes negative, and an unfilled path has no net
    /// at all rather than a zero that would float it over the loss-making ones.
    #[test]
    fn test_rank_outcomes_places_a_loss_making_path_above_an_unfilled_one() {
        let token_out = addr(0x02);
        let mut token_prices = TokenGasPrices::default();
        token_prices.insert(
            token_out.clone(),
            tycho_simulation::tycho_common::simulation::protocol_sim::Price::new(
                BigUint::from(1u64),
                BigUint::from(1u64),
            ),
        );
        let outcomes = vec![
            Some(FullAmountOutcome::Unfilled),
            // Gas costs more than it pays: net output is -400.
            Some(FullAmountOutcome::Filled(hop(100, 500))),
            Some(FullAmountOutcome::Filled(hop(900, 10))),
        ];

        let ranking =
            rank_outcomes(outcomes, &BigUint::from(1u64), Some(&token_prices), &token_out);

        assert_eq!(ranking.by_output, vec![2, 1, 0], "unfilled ranks below a loss-making path");
    }

    /// With no price for the output token gas cannot be converted, so it is not taken off and the
    /// two orderings agree.
    #[test]
    fn test_rank_outcomes_ignores_gas_it_cannot_price() {
        let outcomes = vec![
            Some(FullAmountOutcome::Filled(hop(1000, 500))),
            Some(FullAmountOutcome::Filled(hop(900, 10))),
        ];

        let ranking = rank_outcomes(outcomes, &BigUint::from(1u64), None, &addr(0x02));

        assert_eq!(ranking.by_output_net_gas, vec![0, 1]);
    }

    /// Anchors are the most-connected tokens (highest component-edge degree) plus the native-ETH
    /// sentinel, derived from the graph rather than a hardcoded list.
    #[test]
    fn test_derive_anchor_tokens_ranks_hub_and_includes_native_sentinel() {
        let hub = token_with_decimals(0x01, "HUB", 18);
        let a = token_with_decimals(0x02, "A", 18);
        let b = token_with_decimals(0x03, "B", 18);
        let c = token_with_decimals(0x04, "C", 18);
        let (_market, graph_manager) = setup_market_unweighted_topology(vec![
            ("hub_a", &hub, &a, Box::new(v2_component(1, 1)) as Box<dyn ProtocolSim>),
            ("hub_b", &hub, &b, Box::new(v2_component(1, 1)) as Box<dyn ProtocolSim>),
            ("hub_c", &hub, &c, Box::new(v2_component(1, 1)) as Box<dyn ProtocolSim>),
        ]);

        let anchors = derive_anchor_tokens(graph_manager.graph());

        assert!(anchors.contains(&hub.address), "highest-degree token should be anchored");
        assert!(
            anchors.contains(&Address::from([0u8; 20])),
            "native-ETH sentinel should always be anchored",
        );
    }
}
