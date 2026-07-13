//! Bounded, amount-aware candidate discovery for split routing.
//!
//! A Penumbra-inspired frontier search: expand from the sell token with the full order amount,
//! simulate candidate edges live with `ProtocolSim::get_amount_out`, and keep a small number of
//! best states per intermediate token. The search prefers edges into the output token, then
//! explicitly configured connector tokens, then a default anchor set — including the native-ETH
//! sentinel, so WETH -> ETH -> token routes survive where Tycho models native ETH as the zero
//! address.
//!
//! Discovery is generic over the graph's edge weight `W` — it only reads pool `component_id`s — so
//! the portfolio split ([`super::split_exp`]) reuses it on the `DepthAndPrice` graph, unioning the
//! bounded candidate set into its own exhaustive path enumeration. This module was extracted from
//! the removed `split_bounded` algorithm; `docs/algorithms/split-comparison.md` records the
//! head-to-head history that led to its removal.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::OnceLock,
    time::Instant,
};

use num_bigint::{BigInt, BigUint};
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::models::{token::Token, Address},
};

use super::NoPathReason;
use crate::{
    feed::market_data::{MarketDataView, MarketState},
    graph::{petgraph::StableDiGraph, EdgeData, Path},
    types::{ComponentId, Order},
    AlgorithmError,
};

/// Candidate states retained per intermediate token during bounded expansion.
const CANDIDATE_STATES_PER_NODE: usize = 4;
/// Candidate edge expansions from one path state.
const CANDIDATE_EDGES_PER_STATE: usize = 16;
/// Parallel pools kept for an edge directly into the target token.
const CANDIDATE_DIRECT_EDGES_PER_TOKEN: usize = 4;
/// Parallel pools kept for an edge into an anchor or explicitly configured connector token.
const CANDIDATE_CONNECTOR_EDGES_PER_TOKEN: usize = 2;

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

/// Parameters for one bounded candidate-discovery run. Generic callers (e.g. the portfolio split)
/// construct this to reuse the discovery on their own graph weight type.
#[derive(Clone, Copy)]
pub(crate) struct CandidateSearchConfig<'a> {
    pub(crate) min_hops: usize,
    pub(crate) max_hops: usize,
    pub(crate) max_candidates: usize,
    pub(crate) connector_tokens: Option<&'a HashSet<Address>>,
    pub(crate) source_token: &'a Address,
    pub(crate) start: &'a Instant,
    pub(crate) timeout_ms: u64,
}

pub(crate) trait SplitMarketRead {
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

/// Bounded, amount-aware candidate discovery: expand from the sell token with the full amount,
/// simulate frontier edges live, and prefer edges into the target token, configured connector
/// tokens, or the default anchor set (including the native-ETH sentinel).
///
/// Generic over the graph's edge weight `W` — discovery only reads `component_id` — so other split
/// algorithms (e.g. the portfolio split on `StableDiGraph<DepthAndPrice>`) can reuse it. Returns
/// the candidate paths plus their `(index, full-amount gross output)` ranking, best first.
pub(crate) fn find_candidate_paths<'a, W, M>(
    graph: &'a StableDiGraph<W>,
    market: &M,
    order: &Order,
    cfg: CandidateSearchConfig<'_>,
) -> Result<CandidatePathSet<'a, W>, AlgorithmError>
where
    W: Clone,
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

fn expand_candidate_state<'a, W, M>(
    graph: &'a StableDiGraph<W>,
    market: &M,
    cfg: &CandidateSearchConfig<'_>,
    target: NodeIndex,
    state: CandidatePathState<'a, W>,
    found: &mut Vec<(Path<'a, W>, BigUint)>,
    next_by_node: &mut HashMap<NodeIndex, Vec<CandidatePathState<'a, W>>>,
) where
    W: Clone,
    M: SplitMarketRead + ?Sized,
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

fn candidate_edges_for_state<'a, W, M>(
    graph: &'a StableDiGraph<W>,
    market: &M,
    cfg: &CandidateSearchConfig<'_>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
) -> Vec<ScoredEdge<'a, W>>
where
    M: SplitMarketRead + ?Sized,
{
    let mut preferred = score_candidate_edges(graph, market, cfg, target, state, true);
    if preferred.is_empty() {
        preferred = score_candidate_edges(graph, market, cfg, target, state, false);
    }
    select_candidate_edges(preferred, CANDIDATE_EDGES_PER_STATE)
}

fn score_candidate_edges<'a, W, M>(
    graph: &'a StableDiGraph<W>,
    market: &M,
    cfg: &CandidateSearchConfig<'_>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
    preferred_only: bool,
) -> Vec<ScoredEdge<'a, W>>
where
    M: SplitMarketRead + ?Sized,
{
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

fn simulate_edge<W, M>(
    market: &M,
    amount: &BigUint,
    token_in_addr: &Address,
    edge: &EdgeData<W>,
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
    use alloy::primitives::U256;
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

    use super::*;
    use crate::{
        algorithm::test_utils::{addr, setup_market_unweighted, token_with_decimals},
        graph::GraphManager,
        types::quote::OrderSide,
    };

    fn v2_pool(reserve_a: u128, reserve_b: u128) -> UniswapV2State {
        UniswapV2State::new(
            U256::from(reserve_a) * U256::from(10u64).pow(U256::from(18u64)),
            U256::from(reserve_b) * U256::from(10u64).pow(U256::from(18u64)),
        )
    }

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
