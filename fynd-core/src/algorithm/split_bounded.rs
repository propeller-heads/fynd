//! Bounded split-routing algorithm (`split_bounded`).
//!
//! Ported from the `feat/split-shared-routing-quality-light` line of work (there named
//! `bounded_split`) so it can be benchmarked against `split` and `split_probe` on this branch.
//! It is intentionally self-contained: it keeps its own weightless graph type
//! (`StableDiGraph<()>`), its own allocator copies, and a gas-blind net (`combined_net`
//! returns gross output), exactly as authored. Do not fold it into `split.rs` until the
//! variant comparison has picked a winner.
//!
//! The algorithm replaces exhaustive path enumeration with a bounded, amount-aware candidate
//! search inspired by Penumbra's candidate-set routing: expand from the sell token with the
//! full amount, simulate frontier edges live, and prefer edges into the target token,
//! configured connector tokens, or a default anchor set. Allocation and route assembly reuse
//! the split primitives. See `docs/algorithms/split-bounded.md` for design notes and results.
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
    sync::OnceLock,
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::{token::Token, Address},
};

use super::{
    split_primitives::{build_split_route, HopDescriptor, PathAllocation, SimulatedHop},
    Algorithm, AlgorithmConfig, NoPathReason,
};
use crate::{
    derived::{computation::ComputationRequirements, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketDataView, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, EdgeData, Path, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, RouteResult},
    AlgorithmError,
};

/// Maximum candidate paths kept per order after full-size simulation ranking.
///
/// Split runs alongside single-path algorithms in production, so it does not need to preserve every
/// path that could win as a standalone route. Keep this bounded so split remains close to PFW
/// speed.
const DEFAULT_MAX_CANDIDATES: usize = 128;
/// Maximum number of parallel (pool-disjoint) paths in a split.
const DEFAULT_MAX_PATHS: usize = 4;
/// Number of chunks the order is divided into for water-filling.
const DEFAULT_NUM_CHUNKS: usize = 16;
/// Number of top full-amount paths always considered for shared-pool splitting.
const SHARED_FULL_PATHS: usize = 8;
/// Number of full-size-ranked paths probed with the first shared-pool chunk.
const SHARED_MARGIN_PROBE_PATHS: usize = 32;
/// Number of marginal-probe winners added to the shared-pool candidate set.
const SHARED_MARGIN_PATHS: usize = 8;
/// Upper bound on shared-pool candidate paths.
const SHARED_MAX_CANDIDATES: usize = 12;
/// Upper bound on active paths in the shared-pool allocation.
const SHARED_MAX_ACTIVE_PATHS: usize = 4;
/// Number of chunks for shared-pool fill-and-spill.
const SHARED_NUM_CHUNKS: usize = 24;
/// Candidate states retained per intermediate token during bounded expansion.
const CANDIDATE_STATES_PER_NODE: usize = 4;
/// Candidate edge expansions from one path state.
const CANDIDATE_EDGES_PER_STATE: usize = 16;
/// Parallel pools kept for an edge directly into the target token.
const CANDIDATE_DIRECT_EDGES_PER_TOKEN: usize = 4;
/// Parallel pools kept for an edge into an anchor or explicitly configured connector token.
const CANDIDATE_CONNECTOR_EDGES_PER_TOKEN: usize = 2;

type PoolStateUpdates = Vec<(ComponentId, Box<dyn ProtocolSim>)>;
type SharedProbe = (BigUint, BigUint, PoolStateUpdates);
type SplitPath<'a> = Path<'a, ()>;
type RankedPathScores = Vec<(usize, BigInt)>;
type CandidatePathSet<'a> = (Vec<SplitPath<'a>>, RankedPathScores);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateSearchMode {
    /// Exhaustive enumeration; constructed only from tests as a reference for the bounded mode.
    #[cfg_attr(not(test), allow(dead_code))]
    Exhaustive,
    Bounded,
}

#[derive(Clone)]
struct CandidatePathState<'a> {
    node: NodeIndex,
    path: SplitPath<'a>,
    amount_out: BigUint,
}

struct ScoredEdge<'a> {
    target: NodeIndex,
    edge: &'a EdgeData<()>,
    amount_out: BigUint,
    priority: u8,
}

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

trait SplitMarketRead {
    fn get_token(&self, address: &Address) -> Option<&Token>;

    fn get_simulation_state(&self, id: &str) -> Option<&dyn ProtocolSim>;
}

impl SplitMarketRead for MarketState {
    fn get_token(&self, address: &Address) -> Option<&Token> {
        self.get_token(address)
    }

    fn get_simulation_state(&self, id: &str) -> Option<&dyn ProtocolSim> {
        self.get_simulation_state(id)
    }
}

impl SplitMarketRead for MarketDataView<'_> {
    fn get_token(&self, address: &Address) -> Option<&Token> {
        self.get_token(address)
    }

    fn get_simulation_state(&self, id: &str) -> Option<&dyn ProtocolSim> {
        self.get_simulation_state(id)
    }
}

struct SplitEvalContext<'a> {
    market: &'a MarketState,
    amount_in: &'a BigUint,
    gas_price: &'a BigUint,
    token_out: &'a Address,
    start: &'a Instant,
    timeout_ms: u64,
}

impl SplitEvalContext<'_> {
    fn timed_out(&self) -> bool {
        self.start.elapsed().as_millis() as u64 > self.timeout_ms
    }
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

/// Shared engine behind [`SplitBoundedAlgorithm`]. The exhaustive mode exists only as a
/// test reference; production entry is the bounded mode via `SplitBoundedAlgorithm`.
struct BoundedSplitEngine {
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
    candidate_search: CandidateSearchMode,
}

impl BoundedSplitEngine {
    /// Creates an exhaustive-search engine, used by tests as the reference allocator.
    #[cfg(test)]
    fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Self::with_candidate_search(config, CandidateSearchMode::Exhaustive)
    }

    fn with_candidate_search(
        config: AlgorithmConfig,
        candidate_search: CandidateSearchMode,
    ) -> Result<Self, AlgorithmError> {
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
            candidate_search,
        })
    }

    fn find_paths<'a>(
        graph: &'a StableDiGraph<()>,
        order: &Order,
        min_hops: usize,
        max_hops: usize,
        connector_tokens: Option<&HashSet<Address>>,
    ) -> Result<Vec<SplitPath<'a>>, AlgorithmError> {
        if min_hops == 0 || min_hops > max_hops {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: format!(
                    "invalid hop configuration: min_hops={min_hops} max_hops={max_hops}",
                ),
            });
        }

        let from_idx = graph
            .node_indices()
            .find(|&node| &graph[node] == order.token_in())
            .ok_or(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::SourceTokenNotInGraph,
            })?;
        let to_idx = graph
            .node_indices()
            .find(|&node| &graph[node] == order.token_out())
            .ok_or(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
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

                let already_visited = current_path.tokens.contains(&next_addr);
                let is_closing_circular_route = from_idx == to_idx && next_node == to_idx;
                if already_visited && !is_closing_circular_route {
                    continue;
                }

                let is_destination = next_node == to_idx;
                if !is_destination {
                    if let Some(tokens) = connector_tokens {
                        if !tokens.contains(next_addr) {
                            continue;
                        }
                    }
                }

                let mut new_path = current_path.clone();
                new_path.add_hop(&graph[current_node], edge.weight(), next_addr);

                if next_node == to_idx && new_path.len() >= min_hops {
                    paths.push(new_path.clone());
                }

                queue.push_back((next_node, new_path));
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

    fn find_candidate_paths<'a, M>(
        graph: &'a StableDiGraph<()>,
        market: &M,
        order: &Order,
        cfg: CandidateSearchConfig<'_>,
    ) -> Result<CandidatePathSet<'a>, AlgorithmError>
    where
        M: SplitMarketRead + ?Sized,
    {
        if cfg.min_hops == 0 || cfg.min_hops > cfg.max_hops {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: format!(
                    "invalid hop configuration: min_hops={} max_hops={}",
                    cfg.min_hops, cfg.max_hops,
                ),
            });
        }
        let from_idx = Self::find_token_node(
            graph,
            order.token_in(),
            NoPathReason::SourceTokenNotInGraph,
            order,
        )?;
        let to_idx = Self::find_token_node(
            graph,
            order.token_out(),
            NoPathReason::DestinationTokenNotInGraph,
            order,
        )?;

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
            let mut next_by_node: HashMap<NodeIndex, Vec<CandidatePathState<'a>>> = HashMap::new();
            for state in frontier {
                if state.node == to_idx && from_idx != to_idx {
                    continue;
                }
                Self::expand_candidate_state(
                    graph,
                    market,
                    &cfg,
                    to_idx,
                    state,
                    &mut found,
                    &mut next_by_node,
                );
            }
            frontier = Self::prune_candidate_frontier(next_by_node);
        }

        Self::rank_found_candidate_paths(found, cfg.max_candidates, order)
    }

    fn find_token_node(
        graph: &StableDiGraph<()>,
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

    fn expand_candidate_state<'a, M>(
        graph: &'a StableDiGraph<()>,
        market: &M,
        cfg: &CandidateSearchConfig<'_>,
        target: NodeIndex,
        state: CandidatePathState<'a>,
        found: &mut Vec<(SplitPath<'a>, BigUint)>,
        next_by_node: &mut HashMap<NodeIndex, Vec<CandidatePathState<'a>>>,
    ) where
        M: SplitMarketRead + ?Sized,
    {
        let edges = Self::candidate_edges_for_state(graph, market, cfg, target, &state);
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

    fn candidate_edges_for_state<'a, M>(
        graph: &'a StableDiGraph<()>,
        market: &M,
        cfg: &CandidateSearchConfig<'_>,
        target: NodeIndex,
        state: &CandidatePathState<'a>,
    ) -> Vec<ScoredEdge<'a>>
    where
        M: SplitMarketRead + ?Sized,
    {
        let mut preferred = Self::score_candidate_edges(graph, market, cfg, target, state, true);
        if preferred.is_empty() {
            preferred = Self::score_candidate_edges(graph, market, cfg, target, state, false);
        }
        Self::select_candidate_edges(preferred, CANDIDATE_EDGES_PER_STATE)
    }

    fn score_candidate_edges<'a, M>(
        graph: &'a StableDiGraph<()>,
        market: &M,
        cfg: &CandidateSearchConfig<'_>,
        target: NodeIndex,
        state: &CandidatePathState<'a>,
        preferred_only: bool,
    ) -> Vec<ScoredEdge<'a>>
    where
        M: SplitMarketRead + ?Sized,
    {
        let mut scored = Vec::new();
        for edge in graph.edges(state.node) {
            let next_node = edge.target();
            if !Self::can_extend_path(graph, state, next_node, target, edge.weight(), cfg) {
                continue;
            }
            let priority = match Self::candidate_priority(graph, next_node, target, cfg) {
                Some(priority) => priority,
                None if preferred_only => continue,
                None => 3,
            };
            let Some(amount_out) = Self::simulate_edge(
                market,
                &state.amount_out,
                &graph[state.node],
                edge.weight(),
                &graph[next_node],
            ) else {
                continue;
            };
            scored.push(ScoredEdge {
                target: next_node,
                edge: edge.weight(),
                amount_out,
                priority,
            });
        }
        scored
    }

    fn candidate_priority(
        graph: &StableDiGraph<()>,
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

    fn can_extend_path(
        graph: &StableDiGraph<()>,
        state: &CandidatePathState<'_>,
        next_node: NodeIndex,
        target: NodeIndex,
        edge: &EdgeData<()>,
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

    fn simulate_edge<M>(
        market: &M,
        amount: &BigUint,
        token_in_addr: &Address,
        edge: &EdgeData<()>,
        token_out_addr: &Address,
    ) -> Option<BigUint>
    where
        M: SplitMarketRead + ?Sized,
    {
        let token_in = market.get_token(token_in_addr)?;
        let token_out = market.get_token(token_out_addr)?;
        let state = market.get_simulation_state(&edge.component_id)?;
        state
            .get_amount_out(amount.clone(), token_in, token_out)
            .ok()
            .map(|result| result.amount)
    }

    fn select_candidate_edges(
        mut scored: Vec<ScoredEdge<'_>>,
        max_edges: usize,
    ) -> Vec<ScoredEdge<'_>> {
        scored.sort_by(Self::compare_scored_edges);
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

    fn compare_scored_edges(a: &ScoredEdge<'_>, b: &ScoredEdge<'_>) -> Ordering {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.amount_out.cmp(&a.amount_out))
    }

    fn prune_candidate_frontier<'a>(
        by_node: HashMap<NodeIndex, Vec<CandidatePathState<'a>>>,
    ) -> Vec<CandidatePathState<'a>> {
        by_node
            .into_values()
            .flat_map(|mut states| {
                states.sort_by(|a, b| b.amount_out.cmp(&a.amount_out));
                states.truncate(CANDIDATE_STATES_PER_NODE);
                states
            })
            .collect()
    }

    fn rank_found_candidate_paths<'a>(
        mut found: Vec<(SplitPath<'a>, BigUint)>,
        max_candidates: usize,
        order: &Order,
    ) -> Result<CandidatePathSet<'a>, AlgorithmError> {
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

    fn ranked_simulatable_paths<M>(
        paths: &[SplitPath<'_>],
        market: &M,
        amount_in: &BigUint,
        start: &Instant,
        timeout_ms: u64,
    ) -> Vec<(usize, BigInt)>
    where
        M: SplitMarketRead + ?Sized,
    {
        let mut ranked = Vec::new();

        for (idx, path) in paths.iter().enumerate() {
            if timed_out(start, timeout_ms) {
                break;
            }
            let Some((gross, _gas)) = Self::simulate_amount(path, market, amount_in.clone()) else {
                continue;
            };
            ranked.push((idx, BigInt::from(gross)));
        }

        ranked.sort_by(|(_, a), (_, b)| b.cmp(a));
        ranked
    }

    fn choose_best_split(
        candidates: [Option<RouteResult>; 2],
        single_path_floor: &BigInt,
    ) -> Option<RouteResult> {
        let mut best_route: Option<RouteResult> = None;
        for candidate in candidates.into_iter().flatten() {
            if best_route
                .as_ref()
                .map(|best| candidate.net_amount_out() > best.net_amount_out())
                .unwrap_or(true)
            {
                best_route = Some(candidate);
            }
        }
        best_route.filter(|route| route.net_amount_out() > single_path_floor)
    }

    /// Simulates a single path at `amount`, returning `(gross_output, total_gas)`.
    ///
    /// Handles intra-path pool reuse via per-pool state overrides, like
    /// the single-path simulator, but without allocating `Swap`s — used for the many marginal
    /// probes during water-filling.
    fn simulate_amount<M>(
        path: &SplitPath<'_>,
        market: &M,
        amount: BigUint,
    ) -> Option<(BigUint, BigUint)>
    where
        M: SplitMarketRead + ?Sized,
    {
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

    /// Greedily selects pool-disjoint paths from `ranked` (best first), up to `max_paths`.
    fn select_disjoint<'a>(ranked: &[(usize, &'a SplitPath<'a>)], max_paths: usize) -> Vec<usize> {
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
        _ctx: &SplitEvalContext<'_>,
        total_gross: BigUint,
        _total_gas: &BigUint,
    ) -> BigInt {
        BigInt::from(total_gross)
    }

    fn simulate_on_overrides(
        path: &SplitPath<'_>,
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
        path: &SplitPath<'_>,
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

    fn component_ids_for_paths(paths: &[SplitPath<'_>], indices: &[usize]) -> HashSet<ComponentId> {
        indices
            .iter()
            .flat_map(|idx| paths[*idx].edge_iter())
            .map(|edge| edge.component_id.clone())
            .collect()
    }

    fn select_shared_candidates(
        paths: &[SplitPath<'_>],
        ranked_path_indices: &[usize],
        first_chunk: &BigUint,
        ctx: &SplitEvalContext<'_>,
    ) -> Vec<usize> {
        let mut candidates = Vec::with_capacity(SHARED_MAX_CANDIDATES);
        for idx in ranked_path_indices
            .iter()
            .take(SHARED_FULL_PATHS)
        {
            Self::push_unique(&mut candidates, *idx);
        }

        let empty_overrides = HashMap::new();
        let mut marginal = Vec::new();
        for idx in ranked_path_indices
            .iter()
            .take(SHARED_MARGIN_PROBE_PATHS)
        {
            if ctx.timed_out() {
                break;
            }
            let path = &paths[*idx];
            let Some((out, _gas, _)) =
                Self::simulate_on_overrides(path, ctx, &empty_overrides, first_chunk.clone())
            else {
                continue;
            };
            marginal.push((*idx, BigInt::from(out)));
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
        paths: &[SplitPath<'_>],
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

        for chunk in chunks {
            if ctx.timed_out() {
                break;
            }
            let mut best: Option<(usize, BigInt, BigUint)> = None;
            for (i, &path_idx) in selected.iter().enumerate() {
                let probe_in = &alloc[i] + &chunk;
                let Some((probe_out, _probe_gas)) =
                    Self::simulate_amount(&paths[path_idx], ctx.market, probe_in)
                else {
                    continue;
                };
                let gross_marginal =
                    BigInt::from(probe_out.clone()) - BigInt::from(cur_out[i].clone());
                let net_marginal = gross_marginal;
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
        }

        Self::assemble_disjoint_route(paths, selected, &alloc, ctx, order)
    }

    fn assemble_disjoint_route(
        paths: &[SplitPath<'_>],
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
        paths: &[SplitPath<'_>],
        ranked_path_indices: &[usize],
        ctx: &SplitEvalContext<'_>,
        order: &Order,
    ) -> Option<RouteResult> {
        let chunks = Self::chunks(ctx.amount_in, SHARED_NUM_CHUNKS);
        if chunks.is_empty() {
            return None;
        }
        let candidates =
            Self::select_shared_candidates(paths, ranked_path_indices, &chunks[0], ctx);
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
                let Some((out, _gas, updates)) =
                    Self::simulate_on_overrides(&paths[path_idx], ctx, &overrides, chunk.clone())
                else {
                    continue;
                };
                let net_marginal = BigInt::from(out);
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
        paths: &[SplitPath<'_>],
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
        path: &SplitPath<'_>,
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
}

impl Algorithm for BoundedSplitEngine {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "split_bounded_engine"
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

        let _ = derived;
        let amount_in = order.amount().clone();
        let timeout_ms = self.timeout.as_millis() as u64;
        let (paths, ranked_path_scores, market) = {
            let view = match label.as_ref() {
                Some(label) => market
                    .read_labeled(label)
                    .await
                    .map_err(|err| AlgorithmError::Other(err.to_string()))?,
                None => market.read().await,
            };
            if view.gas_price().is_none() {
                return Err(AlgorithmError::DataNotFound { kind: "gas price", id: None });
            }
            match self.candidate_search {
                CandidateSearchMode::Exhaustive => {
                    let paths = Self::find_paths(
                        graph,
                        order,
                        self.min_hops,
                        self.max_hops,
                        self.connector_tokens.as_ref(),
                    )?;
                    let mut ranked_path_scores = Self::ranked_simulatable_paths(
                        &paths, &view, &amount_in, &start, timeout_ms,
                    );
                    ranked_path_scores.truncate(self.max_candidates);
                    let ranked_path_indices: Vec<usize> = ranked_path_scores
                        .iter()
                        .map(|(idx, _)| *idx)
                        .collect();
                    let component_ids = Self::component_ids_for_paths(&paths, &ranked_path_indices);
                    (paths, ranked_path_scores, view.extract_subset_with_overlay(&component_ids))
                }
                CandidateSearchMode::Bounded => {
                    let (paths, ranked_path_scores) = Self::find_candidate_paths(
                        graph,
                        &view,
                        order,
                        CandidateSearchConfig {
                            min_hops: self.min_hops,
                            max_hops: self.max_hops,
                            max_candidates: self.max_candidates,
                            connector_tokens: self.connector_tokens.as_ref(),
                            source_token: order.token_in(),
                            start: &start,
                            timeout_ms,
                        },
                    )?;
                    let ranked_path_indices: Vec<usize> = ranked_path_scores
                        .iter()
                        .map(|(idx, _)| *idx)
                        .collect();
                    let component_ids = Self::component_ids_for_paths(&paths, &ranked_path_indices);
                    (paths, ranked_path_scores, view.extract_subset_with_overlay(&component_ids))
                }
            }
        };
        if ranked_path_scores.is_empty() {
            return Err(AlgorithmError::InsufficientLiquidity);
        }
        let single_path_floor = ranked_path_scores[0].1.clone();
        let ranked_path_indices: Vec<usize> = ranked_path_scores
            .iter()
            .map(|(idx, _)| *idx)
            .collect();
        let component_ids = Self::component_ids_for_paths(&paths, &ranked_path_indices);
        let market = market.extract_subset(&component_ids);
        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();
        let ctx = SplitEvalContext {
            market: &market,
            amount_in: &amount_in,
            gas_price: &gas_price,
            token_out: order.token_out(),
            start: &start,
            timeout_ms,
        };

        let ranked: Vec<(usize, &SplitPath<'_>)> = ranked_path_indices
            .iter()
            .map(|idx| (*idx, &paths[*idx]))
            .collect();
        let disjoint = Self::select_disjoint(&ranked, self.max_paths);

        let disjoint_candidate =
            Self::build_disjoint_route(&paths, &disjoint, &ctx, order, self.num_chunks);
        let shared_candidate = Self::fill_and_spill(&paths, &ranked_path_indices, &ctx, order);
        Self::choose_best_split([disjoint_candidate, shared_candidate], &single_path_floor)
            .ok_or(AlgorithmError::InsufficientLiquidity)
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        ComputationRequirements::none()
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Split-routing algorithm with bounded Penumbra-inspired candidate discovery.
///
/// This uses the same split allocators and route assembly as `BoundedSplitEngine`, but replaces
/// exhaustive path enumeration with a bounded direct, connector, and anchor-token expansion.
pub struct SplitBoundedAlgorithm {
    inner: BoundedSplitEngine,
}

impl SplitBoundedAlgorithm {
    /// Creates a new `SplitBoundedAlgorithm` from an [`AlgorithmConfig`].
    pub(crate) fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        Ok(Self {
            inner: BoundedSplitEngine::with_candidate_search(config, CandidateSearchMode::Bounded)?,
        })
    }
}

impl Algorithm for SplitBoundedAlgorithm {
    type GraphType = StableDiGraph<()>;
    type GraphManager = PetgraphStableDiGraphManager<()>;

    fn name(&self) -> &str {
        "split_bounded"
    }

    async fn find_best_route(
        &self,
        graph: &Self::GraphType,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        self.inner
            .find_best_route(graph, market, label, derived, order)
            .await
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        self.inner.computation_requirements()
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
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

    fn setup_market(
        pools: Vec<(&str, Token, Token, Box<dyn ProtocolSim>)>,
    ) -> (MarketData, PetgraphStableDiGraphManager<()>) {
        let mut market = MarketState::new();
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });
        market.update_last_updated(BlockInfo::new(1, "0x01".to_string(), 0));

        for (pool_id, token_a, token_b, state) in pools {
            let tokens = vec![token_a.clone(), token_b.clone()];
            market.upsert_components(std::iter::once(component(pool_id, &tokens)));
            market.upsert_tokens(tokens);
            market.update_states([(pool_id.to_string(), state)]);
        }

        let mut graph_manager = PetgraphStableDiGraphManager::<()>::default();
        graph_manager.initialize_graph(&market.component_topology());

        (MarketData::new(Arc::new(RwLock::new(market))), graph_manager)
    }

    async fn solve_split_route(
        market: MarketData,
        graph_manager: &PetgraphStableDiGraphManager<()>,
        order: &Order,
        config: AlgorithmConfig,
    ) -> RouteResult {
        let algo = BoundedSplitEngine::with_config(config).unwrap();
        algo.find_best_route(graph_manager.graph(), market, None, None, order)
            .await
            .expect("split route solves")
    }

    async fn best_single_path_output(
        market: &MarketData,
        graph_manager: &PetgraphStableDiGraphManager<()>,
        order: &Order,
        max_hops: usize,
    ) -> BigInt {
        let paths = BoundedSplitEngine::find_paths(graph_manager.graph(), order, 1, max_hops, None)
            .expect("paths exist");
        let view = market.read().await;
        let ranked = BoundedSplitEngine::ranked_simulatable_paths(
            &paths,
            &view,
            order.amount(),
            &Instant::now(),
            2000,
        );
        ranked
            .first()
            .map(|(_, amount)| amount.clone())
            .expect("at least one simulatable path")
    }

    /// Two equally-deep WETH/USDC pools: a large order should split ~50/50 and beat any single
    /// path.
    #[tokio::test]
    async fn split_beats_single_path_on_two_equal_pools() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (market, graph_manager) = setup_market(vec![
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

        let single = best_single_path_output(&market, &graph_manager, &order, 3).await;
        let split = BoundedSplitEngine::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market.clone(), None, None, &order)
            .await
            .expect("split solves");

        let split_paths = split
            .route()
            .swaps()
            .iter()
            .filter(|swap| swap.token_in() == &weth.address)
            .count();
        assert_eq!(split_paths, 2, "large order should use both pools");
        assert!(
            split.net_amount_out() > &single,
            "split ({}) should beat single-path ({})",
            split.net_amount_out(),
            single
        );

        // Splitting 50/50 across two identical pools should be ~20% better than one pool here.
        let gain = split.net_amount_out().to_f64().unwrap() / single.to_f64().unwrap();
        assert!(gain > 1.15, "expected >15% gain from splitting, got {:.3}x", gain);
    }

    /// Two candidate paths share the same first pool, then diverge across shallow downstream pools.
    /// Pool-disjoint splitting cannot use both paths, but shared-pool fill-and-spill can.
    #[tokio::test]
    async fn split_uses_shared_prefix_when_downstream_liquidity_splits() {
        let src = token_with_decimals(0x01, "SRC", 18);
        let bridge = token_with_decimals(0x02, "BRG", 18);
        let dst = token_with_decimals(0x03, "DST", 18);
        let (market, graph_manager) = setup_market(vec![
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

        let single = best_single_path_output(&market, &graph_manager, &order, 3).await;
        let split = BoundedSplitEngine::with_config(config.clone())
            .unwrap()
            .find_best_route(graph_manager.graph(), market.clone(), None, None, &order)
            .await
            .expect("split solves");

        assert!(
            split.net_amount_out() > &single,
            "shared-prefix split ({}) should beat single-path ({})",
            split.net_amount_out(),
            single
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

    #[tokio::test]
    async fn candidate_ranking_uses_simulation_without_edge_weights() {
        let link = token_with_decimals(0x01, "LINK", 18);
        let weth = token_with_decimals(0x02, "WETH", 18);
        let (market, graph_manager) = setup_market(vec![
            (
                "a_weak_link_weth",
                link.clone(),
                weth.clone(),
                Box::new(v2_pool(2_000_000, 18, 264, 18)) as Box<dyn ProtocolSim>,
            ),
            (
                "z_strong_link_weth",
                link.clone(),
                weth.clone(),
                Box::new(v2_pool(2_000_000, 18, 5_700, 18)) as Box<dyn ProtocolSim>,
            ),
        ]);
        let order = Order::new(
            link.address.clone(),
            weth.address.clone(),
            BigUint::from(1_000u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );

        let paths = BoundedSplitEngine::find_paths(graph_manager.graph(), &order, 1, 1, None)
            .expect("paths exist");
        let view = market.read().await;
        let ranked = BoundedSplitEngine::ranked_simulatable_paths(
            &paths,
            &view,
            order.amount(),
            &Instant::now(),
            2000,
        );
        let best_path = &paths[ranked[0].0];
        let best_component = &best_path.edge_iter()[0].component_id;

        assert_eq!(
            best_component, "z_strong_link_weth",
            "split should rank by simulated output, not by topology or derived edge weights"
        );
    }

    /// Split is not a single-path fallback. Production pools should run a single-path algorithm
    /// alongside it and let the worker router choose the best result.
    #[tokio::test]
    async fn single_path_market_returns_no_split_route() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (market, graph_manager) = setup_market(vec![(
            "pool_a",
            weth.clone(),
            usdc.clone(),
            Box::new(weth_usdc_pool(1000, 3_000_000)) as Box<dyn ProtocolSim>,
        )]);

        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();

        let result = BoundedSplitEngine::with_config(config)
            .unwrap()
            .find_best_route(graph_manager.graph(), market, None, None, &order)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::InsufficientLiquidity)),
            "single-path-only market should not produce a split route: {result:?}"
        );
    }

    #[tokio::test]
    async fn split_bounded_solves_with_bounded_candidate_search() {
        let weth = token_with_decimals(0x01, "WETH", 18);
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let (market, graph_manager) = setup_market(vec![
            (
                "pool_a",
                weth.clone(),
                usdc.clone(),
                Box::new(weth_usdc_pool(1_000, 2_000_000)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_b",
                weth.clone(),
                usdc.clone(),
                Box::new(weth_usdc_pool(1_000, 2_000_000)) as Box<dyn ProtocolSim>,
            ),
        ]);
        let order = Order::new(
            weth.address.clone(),
            usdc.address.clone(),
            BigUint::from(500u64) * BigUint::from(10u64).pow(18),
            OrderSide::Sell,
            addr(0xFF),
        );
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();

        let algorithm = SplitBoundedAlgorithm::with_config(config).unwrap();
        assert_eq!(algorithm.name(), "split_bounded");

        let bounded = algorithm
            .find_best_route(graph_manager.graph(), market, None, None, &order)
            .await
            .expect("bounded split solves");

        let split_paths = bounded
            .route()
            .swaps()
            .iter()
            .filter(|swap| swap.token_in() == &weth.address)
            .count();
        assert_eq!(split_paths, 2, "bounded split should use both pools");
    }

    #[test]
    fn split_has_no_derived_requirements() {
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();
        let requirements = BoundedSplitEngine::with_config(config)
            .unwrap()
            .computation_requirements();

        assert!(
            !requirements.has_requirements(),
            "split should not wait for or update from derived data"
        );
    }

    #[test]
    fn split_bounded_has_no_derived_requirements() {
        let config =
            AlgorithmConfig::new(1, 3, std::time::Duration::from_millis(2000), None).unwrap();
        let requirements = SplitBoundedAlgorithm::with_config(config)
            .unwrap()
            .computation_requirements();

        assert!(
            !requirements.has_requirements(),
            "split_bounded should not wait for or update from derived data"
        );
    }
}
