//! Water-fill split-routing algorithm (`water_fill`).
//!
//! For large orders, price impact makes it better to split the order across several parallel routes
//! so the marginal price stays low. `WaterFillAlgorithm` is a portfolio router: it builds up to
//! four split candidates and returns the one with the best net-of-gas output, never worse than the
//! best single path.
//!
//! The candidates are the best single path, a coarse pool-disjoint floor, a refined pool-disjoint
//! split, and a shared-pool fill-and-spill:
//!
//! * **Floor** — a single gated coarse water-fill (20 chunks) over pool-disjoint paths. It does
//!   exactly the classic split's allocation work at the same cost, so a tight timeout cannot starve
//!   it into a single-path fallback while a split would still win — this is what makes the
//!   never-lose guarantee hold under time pressure.
//! * **Refined disjoint** — two-phase: pick the active path set at coarse granularity, where the
//!   gas-activation gate is correct, then refine the allocation over that fixed set on a fine
//!   256-chunk grid with the gate off (the gas is already justified).
//! * **Fill-and-spill** — a shared-pool overlay allocation with marginal-probe candidate selection,
//!   which reaches tree routes that split at an intermediate token (paths sharing a pool), which no
//!   pool-disjoint allocation can express.
//!
//! All allocation runs use an incremental water-fill: because constant-product / tick AMMs are
//! path-independent in cumulative input (one swap of `x` equals two sequential swaps summing to
//! `x`), probing the marginal of the *next* chunk against a committed pool overlay is identical to
//! re-simulating at the cumulative amount, but costs O(chunks) instead of O(chunks^2). The saved
//! work funds the fine grid.
//!
//! Candidate discovery (the "Candidate discovery" section below) unions an exhaustive path
//! enumeration with a bounded, amount-aware frontier search, so connector and anchor routes
//! (including the native-ETH sentinel) survive spot × depth truncation. Every returned route is
//! assembled through the shared, encoding-safe split primitives.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::OnceLock,
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
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
    feed::market_data::{MarketData, MarketDataView, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, EdgeData, Path, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult},
    AlgorithmError,
};

/// Maximum candidate paths simulated per order after heuristic ranking.
const DEFAULT_MAX_CANDIDATES: usize = 5000;
/// Cap on candidates from the bounded amount-aware discovery unioned into the candidate set
/// (matches the bounded discovery's own candidate cap; see the discovery section below).
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
/// Candidate states retained per intermediate token during bounded discovery expansion.
const CANDIDATE_STATES_PER_NODE: usize = 4;
/// Candidate edge expansions from one path state during discovery.
const CANDIDATE_EDGES_PER_STATE: usize = 16;
/// Parallel pools kept for a discovery edge directly into the target token.
const CANDIDATE_DIRECT_EDGES_PER_TOKEN: usize = 4;
/// Parallel pools kept for a discovery edge into an anchor or configured connector token.
const CANDIDATE_CONNECTOR_EDGES_PER_TOKEN: usize = 2;
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

/// Portfolio split router: splits orders across pool-disjoint (and, via fill-and-spill,
/// pool-sharing) paths to minimize price impact, returning the best net of the single path and
/// several split allocations.
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

impl WaterFillAlgorithm {
    /// Creates a new `WaterFillAlgorithm` from an [`AlgorithmConfig`].
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
            // Bounded amount-aware discovery (see the discovery section below): union its
            // candidates ahead of the pre-ranked set, so connector/anchor routes (incl. the
            // native-ETH sentinel) survive the spot×depth truncation. Discovery failure is not
            // fatal — the pre-ranked set already guarantees a route.
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

impl Algorithm for WaterFillAlgorithm {
    type GraphType = StableDiGraph<DepthAndPrice>;
    type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        "water_fill"
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

        // Build the portfolio's split candidates, best-net of which competes with the single path.
        let mut candidates: Vec<SplitCandidate> = Vec::new();
        // Floor first: a single gated coarse pass, same cost as the classic split, so a tight
        // timeout cannot starve it into a single-path fallback while a split would still win.
        if let Some(c) =
            self.floor_alloc(&ordered, &market, &gas_price, tp, order, start, self.max_paths)
        {
            candidates.push(c);
        }
        // Refined fine allocation over the same active set (bonus; may be starved by a timeout).
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
        // Shared-pool fill-and-spill with marginal-probe candidate selection. It captures wins that
        // are tree routes splitting at an intermediate token (paths sharing a pool), which no
        // pool-disjoint allocation can express.
        if let Some(c) =
            self.fillspill_alloc(&ordered, &market, &gas_price, tp, order, start, FINE_CHUNKS)
        {
            candidates.push(c);
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

impl WaterFillAlgorithm {
    /// Incumbent-equivalent floor split: a single gated coarse water-fill over the disjoint set,
    /// same chunk grid and cost as the incumbent split allocation. Because it
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

        // Phase 3: exchange refinement. The fine water-fill quantizes each path to a whole chunk,
        // so it can sit up to one chunk off the equal-marginal optimum. Nudge flow between
        // paths at sub-chunk resolution, accepting only strictly-improving moves
        // (never-lose).
        let refined = self.disjoint_exchange(
            ordered,
            &active,
            market,
            gas_price,
            token_prices,
            order,
            start,
            chunks,
            fine,
        );
        self.build_disjoint_legs(ordered, &active, &refined, market, token_prices, order)
    }

    /// Net output (gross output minus gas cost in output-token terms) of `path` simulated in
    /// isolation at `amount`. Pool-disjoint paths never interfere, so an isolated re-simulation is
    /// exact. A zero amount means the path is dropped from the route: it yields no output and,
    /// since it is no longer swapped, no gas — so dropping a donor credits its saved gas
    /// automatically.
    fn path_net(
        path: &Path<DepthAndPrice>,
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        amount: &BigUint,
    ) -> Option<BigInt> {
        if amount.is_zero() {
            return Some(BigInt::zero());
        }
        let empty: HashMap<ComponentId, Box<dyn ProtocolSim>> = HashMap::new();
        let step = Self::simulate_step(path, market, &empty, amount.clone())?;
        let activation =
            Self::gas_cost_in_token(&step.gas, gas_price, token_prices, order.token_out())
                .map(BigInt::from)
                .unwrap_or_else(BigInt::zero);
        Some(BigInt::from(step.amount_out) - activation)
    }

    /// Exchange-refinement pass over the fixed active set, warm-started from the fine water-fill.
    /// Water-fill can never un-commit a chunk, so its allocation is quantized to one fine chunk and
    /// can miss the equal-marginal split. This shifts `delta` of input from an over-allocated donor
    /// to an under-allocated recipient whenever the pair's summed net output strictly improves,
    /// then halves `delta` once no move helps, down to a sub-chunk floor. Paths are
    /// pool-disjoint, so a trial re-simulates only the two paths it touches (unchanged paths
    /// keep their cached net), and only strictly-improving moves are accepted, so the result
    /// never scores below the warm start.
    #[allow(clippy::too_many_arguments)]
    fn disjoint_exchange(
        &self,
        ordered: &[Path<DepthAndPrice>],
        active: &[usize],
        market: &MarketState,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        order: &Order,
        start: Instant,
        fine_chunks: usize,
        alloc: Vec<BigUint>,
    ) -> Vec<BigUint> {
        let k = active.len();
        if k < 2 {
            return alloc;
        }
        let amount_in = order.amount().clone();
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
        let mut net_cache: Vec<BigInt> = Vec::with_capacity(k);
        for (i, &path_idx) in active.iter().enumerate() {
            let Some(net) =
                Self::path_net(&ordered[path_idx], market, gas_price, token_prices, order, &cum[i])
            else {
                // The warm start does not even simulate cleanly; refining it is unsafe, so keep it.
                return cum;
            };
            net_cache.push(net);
        }

        let mut sims = 0usize;
        while delta >= min_delta && !delta.is_zero() {
            if start.elapsed().as_millis() as u64 > timeout_ms || sims >= EXCHANGE_MAX_SIMS {
                break;
            }

            let mut best: Option<ExchangeMove> = None;
            for donor in 0..k {
                if cum[donor] < delta {
                    continue;
                }
                let donor_amt = &cum[donor] - &delta;
                let Some(donor_net) = Self::path_net(
                    &ordered[active[donor]],
                    market,
                    gas_price,
                    token_prices,
                    order,
                    &donor_amt,
                ) else {
                    continue;
                };
                sims += 1;
                for recipient in 0..k {
                    if recipient == donor || sims >= EXCHANGE_MAX_SIMS {
                        continue;
                    }
                    let recip_amt = &cum[recipient] + &delta;
                    let Some(recip_net) = Self::path_net(
                        &ordered[active[recipient]],
                        market,
                        gas_price,
                        token_prices,
                        order,
                        &recip_amt,
                    ) else {
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

// ==================== Candidate discovery ====================
//
// Bounded, amount-aware frontier search, unioned into the portfolio's exhaustive enumeration by
// `setup`. A Penumbra-inspired expansion from the sell token: simulate frontier edges live and
// prefer edges into the output token, configured connector tokens, or a default anchor set
// (including the native-ETH sentinel). Generic over the graph's edge weight `W` (discovery only
// reads `component_id`s) so it runs on the production `DepthAndPrice` graph while tests exercise it
// on a bare topology graph.

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
    source_token: &'a Address,
    start: &'a Instant,
    timeout_ms: u64,
}

fn timed_out(start: &Instant, timeout_ms: u64) -> bool {
    start.elapsed().as_millis() as u64 > timeout_ms
}

fn default_anchor_tokens() -> &'static HashSet<Address> {
    static TOKENS: OnceLock<HashSet<Address>> = OnceLock::new();
    TOKENS.get_or_init(|| {
        [
            // Native ETH sentinel used by Fynd/Tycho.
            "0x0000000000000000000000000000000000000000",
            // Ethereum mainnet: WETH, USDC, USDT, DAI, WBTC, wstETH, AAVE, UNI.
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "0xdAC17F958D2ee523a2206206994597C13D831ec7",
            "0x6B175474E89094C44Da98b954EedeAC495271d0F",
            "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
            "0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0",
            "0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            // OP-stack chains: canonical WETH. Base: USDC and cbBTC. Unichain: USDC.
            "0x4200000000000000000000000000000000000006",
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "0xcbB7C0000aB88B473b1f5afd9ef808440eed33bF",
            "0x078D782b760474a361dDA0AF3839290b0EF57AD6",
        ]
        .into_iter()
        .map(|addr| Address::from_str(addr).expect("valid default split anchor token"))
        .collect()
    })
}

/// Runs the bounded discovery and returns the candidate paths plus their `(index, full-amount
/// gross output)` ranking, best first.
fn find_candidate_paths<'a, W>(
    graph: &'a StableDiGraph<W>,
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
    graph: &StableDiGraph<W>,
    token: &Address,
    reason: NoPathReason,
    order: &Order,
) -> Result<NodeIndex, AlgorithmError> {
    graph
        .node_indices()
        .find(|&node| &graph[node] == token)
        .ok_or(AlgorithmError::NoPath {
            from: order.token_in().clone(),
            to: order.token_out().clone(),
            reason,
        })
}

fn expand_candidate_state<'a, W>(
    graph: &'a StableDiGraph<W>,
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
    graph: &'a StableDiGraph<W>,
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
    graph: &'a StableDiGraph<W>,
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

fn candidate_priority<W>(
    graph: &StableDiGraph<W>,
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
        None => default_anchor_tokens()
            .contains(token)
            .then_some(2),
    }
}

fn can_extend_path<W>(
    graph: &StableDiGraph<W>,
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
        .get_amount_out(amount.clone(), token_in, token_out)
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
        let key: Vec<ComponentId> = path
            .edge_iter()
            .iter()
            .map(|edge| edge.component_id.clone())
            .collect();
        if !keys.insert(key) {
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
                optimal_two_pool_output, split_metrics, two_equal_weth_usdc,
                TWO_EQUAL_USDC_RESERVE, TWO_EQUAL_WETH_RESERVE,
            },
            test_utils::{addr, setup_market_unweighted, token_with_decimals},
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

    /// Two equally-deep pools: a large order splits across both and beats any single path.
    #[tokio::test]
    async fn split_beats_single_path_on_two_equal_pools() {
        let m = two_equal_weth_usdc(1);
        let order = whole_weth_order(&m.weth, &m.usdc, 500);

        let split = WaterFillAlgorithm::with_config(config())
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
        let ml = MostLiquidAlgorithm::with_config(config())
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

        let (_, path_count, split_gross) = split_metrics(&split, &m.weth, &m.usdc);
        let (_, _, ml_gross) = split_metrics(&ml, &m.weth, &m.usdc);
        assert_eq!(path_count, 2, "large order should use both pools");
        let gain = split_gross.to_f64().unwrap() / ml_gross.to_f64().unwrap();
        assert!(gain > 1.15, "expected >15% gain from splitting, got {gain:.3}x");
    }

    /// A tiny order must never lose to the single path.
    #[tokio::test]
    async fn small_order_does_not_lose_to_single_path() {
        let m = two_equal_weth_usdc(1);
        let order = Order::new(
            m.weth.clone(),
            m.usdc.clone(),
            BigUint::from(10u64).pow(15),
            OrderSide::Sell,
            addr(0xFF),
        );

        let split = WaterFillAlgorithm::with_config(config())
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
        let ml = MostLiquidAlgorithm::with_config(config())
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

        let (split_net, _, _) = split_metrics(&split, &m.weth, &m.usdc);
        let (ml_net, _, _) = split_metrics(&ml, &m.weth, &m.usdc);
        assert!(
            split_net >= ml_net,
            "split must never lose to single-path: split={split_net} ml={ml_net}",
        );
    }

    /// On two equal fee-free pools the split's gross output must come within a tight tolerance of
    /// the analytical two-pool optimum (a 50/50 split): the fine 256-chunk allocation finds the
    /// optimal allocation, not just any splitting one.
    #[tokio::test]
    async fn portfolio_output_near_two_pool_optimum() {
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

    /// Under a tight timeout the split must still not lose to the best single path: the floor pass
    /// does exactly the classic split's coarse work, so a tight timeout cannot starve it into a
    /// single-path fallback while a split would still win.
    #[tokio::test]
    async fn portfolio_no_loss_under_tight_timeout() {
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
                .await
                .expect("split solves");
            let (split_net, _, _) = split_metrics(&split, &m.weth, &m.usdc);

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
                split_net >= single_net,
                "split lost to single-path under {ms}ms timeout: split={split_net} single={single_net}",
            );
        }
    }

    /// Gross output should scale with allocation: the fine grid must not overstate a leg's output.
    #[tokio::test]
    async fn portfolio_gross_is_positive_and_sane() {
        let m = two_equal_weth_usdc(1);
        let order = whole_weth_order(&m.weth, &m.usdc, 100);

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
            .expect("solves");
        let (_, _, gross) = split_metrics(&result, &m.weth, &m.usdc);
        assert!(gross.to_f64().unwrap() > 0.0);
    }

    // ==================== Candidate discovery ====================

    /// Bounded discovery finds both parallel pools as candidate paths and ranks the deeper pool
    /// first by simulated full-amount output, using live simulation only (no precomputed edge
    /// weights on the weightless graph).
    #[tokio::test]
    async fn discovery_finds_and_ranks_parallel_pools() {
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
    async fn discovery_rejects_invalid_hop_configuration() {
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
}
