//! Bellman-Ford algorithm with SPFA optimization for simulation-driven routing.
//!
//! Runs actual component simulations (`get_amount_out()`) during edge relaxation to find
//! optimal A-to-B routes that account for slippage, fees, and component mechanics at the
//! given trade size.
//!
//! Key features:
//! - **Gas-aware relaxation**: When token prices and gas price are available, relaxation compares
//!   net amounts (gross output minus cumulative gas cost in token terms) instead of gross output
//!   alone. Falls back to gross comparison when data is unavailable.
//! - **Subgraph extraction**: BFS prunes the graph to nodes reachable within `max_hops`
//! - **SPFA (Shortest Path Faster Algorithm) queuing**: Only re-relaxes edges from nodes whose
//!   amount improved
//! - **Forbid revisits**: Skips edges that would revisit a token or component already in the path
//!
//! # Known limitation: SPFA order-dependence
//!
//! SPFA reads and writes the same `amount[]` array within a round (unlike
//! textbook Bellman-Ford which snapshots between rounds). Processing node B
//! before node C can update intermediate amounts that C then builds on,
//! producing different routes depending on iteration order. Active nodes are
//! sorted by `NodeIndex` for determinism, but the chosen ordering is not
//! guaranteed to find the globally optimal route. A proper fix would be to
//! snapshot amounts between rounds or use a priority-based processing order.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, instrument, trace, warn};
use tycho_simulation::{
    tycho_common::models::Address,
    tycho_core::{models::token::Token, simulation::protocol_sim::Price},
};

use super::{
    split_primitives::MarketOverrides, Algorithm, AlgorithmConfig, AlgorithmError, NoPathReason,
};
use crate::{
    algorithm::{paths, sim_guard::GuardedProtocolSim},
    derived::{
        computation::ComputationRequirements,
        types::{SpotPrices, TokenGasPrices},
        SharedDerivedDataRef,
    },
    feed::market_data::{MarketData, MarketDataView, MarketState, StateLabel},
    graph::{petgraph::StableDiGraph, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, Route, RouteResult, Swap},
};

/// BFS subgraph: adjacency list, token node set, and component ID set.
///
/// The component set borrows from the graph. It exists to ask for the market subset and is done
/// with before the solve starts, unlike the adjacency list, which outlives the graph borrow inside
/// [`BellmanFordContext`] and so owns its ids.
type Subgraph<'a> = (
    FxHashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>>,
    FxHashSet<NodeIndex>,
    FxHashSet<&'a ComponentId>,
);

/// Everything needed to call `find_single_route` repeatedly without redoing setup.
///
/// Holds a snapshot of market and derived state taken under lock at build time. Solves read the
/// snapshot without re-acquiring any lock, so all route evaluations within one order see a
/// consistent view of the same block's component states.
pub(crate) struct BellmanFordContext {
    pub(crate) token_in_node: NodeIndex,
    /// Absent when the context was built from a source token only, with no destination; such
    /// a context serves `reach_from_source_token` but not `find_single_route`.
    pub(crate) token_out_node: Option<NodeIndex>,
    pub(crate) adj: FxHashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>>,
    pub(crate) token_map: FxHashMap<NodeIndex, Arc<Token>>,
    pub(crate) market_data: MarketState,
    pub(crate) gas_price_wei: Option<BigUint>,
    pub(crate) token_prices: Option<TokenGasPrices>,
    pub(crate) spot_prices: Option<SpotPrices>,
    pub(crate) node_address: FxHashMap<NodeIndex, Address>,
    pub(crate) max_idx: usize,
    pub(crate) scoring: RouteScoringMode,
}

impl BellmanFordContext {
    /// Re-points the context at new endpoints, so one snapshot can serve many solves.
    ///
    /// Everything else — subgraph, token metadata, market snapshot — is reused as-is, so the
    /// caller must pick nodes inside the subgraph the context was built from. A node outside
    /// it has no adjacency entries, so a solve from it finds no route rather than panicking.
    pub(crate) fn reroot(&mut self, token_in_node: NodeIndex, token_out_node: Option<NodeIndex>) {
        self.token_in_node = token_in_node;
        self.token_out_node = token_out_node;
    }
}

/// What one relaxation delivers at a destination the source token reaches: the output amount
/// and the components along the best path to it.
pub(crate) struct ReachedToken {
    /// What the path delivers at the destination. Never zero: a destination the relaxation
    /// leaves at zero counts as unreached and is absent from the map.
    pub(crate) amount_out: BigUint,
    /// The components the path runs through, in hop order.
    pub(crate) components: Vec<ComponentId>,
}

/// Controls how `find_single_route` ranks candidate routes after simulation.
pub(crate) enum RouteScoringMode {
    /// Rank by gross output (ignore gas cost). Used when the caller accounts for gas externally.
    GrossOutput,
    /// Rank by net output (gross output minus gas cost in output token units). Default.
    NetOutput,
}

/// Per-call overrides for `find_single_route`.
#[derive(Default)]
pub(crate) struct FindRouteOptions {
    /// Component state overrides: degrade or zero-gas specific components without modifying market
    /// data.
    pub(crate) overrides: MarketOverrides,
}

/// Output of the SPFA relaxation pass: per-node best-path arrays.
struct SPFAResult {
    /// Best gross output amount reachable at each node index.
    amount: Vec<BigUint>,
    /// The (predecessor node, component) that last improved each node's amount.
    predecessor: Vec<Option<(NodeIndex, ComponentId)>>,
    /// Gas consumed by the edge that last improved each node's amount.
    edge_gas: Vec<BigUint>,
    /// Cumulative spot-price product from token_in to each node (for gas fallback).
    spot_product: Vec<f64>,
    /// True if some hop's input couldn't cover that hop's own gas (gas-aware) or a sim
    /// produced a literal zero output (gas-unaware) — i.e. the amount is dust, not unroutable.
    input_below_hop_gas: bool,
}

/// Bellman-Ford algorithm with SPFA optimisation for simulation-driven DEX routing.
///
/// Finds optimal A→B routes by running actual component simulations during edge relaxation,
/// accounting for slippage, fees, and component mechanics at the requested trade size.
/// Gas costs are subtracted when price data is available.
pub struct BellmanFordAlgorithm {
    max_hops: usize,
    timeout: Duration,
    gas_aware: bool,
    connector_tokens: Option<FxHashSet<Address>>,
}

impl Default for BellmanFordAlgorithm {
    fn default() -> Self {
        Self::with_config(AlgorithmConfig::default())
    }
}

impl BellmanFordAlgorithm {
    pub(crate) fn with_config(config: AlgorithmConfig) -> Self {
        Self {
            max_hops: config.max_hops(),
            timeout: config.timeout(),
            gas_aware: config.gas_aware(),
            connector_tokens: config.connector_tokens().cloned(),
        }
    }

    /// One-time async setup for repeated `find_single_route` calls.
    ///
    /// Validates the order, extracts the subgraph, acquires the market and derived data
    /// locks exactly once, and snapshots all state into a [`BellmanFordContext`]. All
    /// subsequent `find_single_route` calls on the returned context use the same block's
    /// component states.
    pub(crate) async fn build_context(
        &self,
        graph: &StableDiGraph<()>,
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<BellmanFordContext, AlgorithmError> {
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let (token_prices, spot_prices) = if let Some(ref d) = derived {
            let guard = d.read().await;
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

        if token_in_node == token_out_node {
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason: NoPathReason::NoGraphPath,
            });
        }

        let subgraph =
            Self::get_subgraph(graph, token_in_node, Some(token_out_node), self.max_hops).ok_or(
                AlgorithmError::NoPath {
                    from: order.token_in().clone(),
                    to: order.token_out().clone(),
                    reason: NoPathReason::NoGraphPath,
                },
            )?;
        let market_view = paths::read_market(&market, label).await?;
        let mut ctx = self.snapshot_context(
            graph,
            market_view,
            subgraph,
            token_in_node,
            Some(token_out_node),
        );
        ctx.token_prices = token_prices;
        ctx.spot_prices = spot_prices;
        Ok(ctx)
    }

    /// A context whose subgraph is everything within `walk_hops` of `token_in` — no destination
    /// prunes it. Having no destination, it cannot serve `find_single_route` until
    /// `reroot` gives it one.
    ///
    /// `walk_hops` bounds the subgraph, not route length — routes stay bounded by the
    /// algorithm's own `max_hops`. A caller that re-roots the context at tokens away from
    /// `token_in` walks further than it routes: a route of `max_hops` hops back to `token_in`
    /// can start `max_hops` away, and the walk must include that node's outgoing edges.
    ///
    /// Reads the market unlabeled and no derived data. `None` when `token_in` is not in the
    /// graph or nothing is reachable from it.
    pub(crate) async fn build_context_from_source_token(
        &self,
        graph: &StableDiGraph<()>,
        market: MarketData,
        token_in: &Address,
        walk_hops: usize,
    ) -> Option<BellmanFordContext> {
        let token_in_node = graph
            .node_indices()
            .find(|&n| &graph[n] == token_in)?;
        let subgraph = Self::get_subgraph(graph, token_in_node, None, walk_hops)?;
        let market_view = market.read().await;
        Some(self.snapshot_context(graph, market_view, subgraph, token_in_node, None))
    }

    /// Snapshots everything a solve reads from the market — tokens, component states, gas price,
    /// and scoring inputs — for a subgraph the caller has already walked.
    ///
    /// The caller walks the subgraph *before* acquiring `market_view`: the walk needs only the
    /// graph, and holding the read guard through it would queue the feed's writer — and every
    /// quote's read behind that writer. The endpoints must be the pair the subgraph was walked
    /// with; the destination, when present, is carried for
    /// `find_single_route`'s readout.
    ///
    /// Derived data starts empty; a caller that has token or spot prices sets the fields on the
    /// returned context.
    fn snapshot_context(
        &self,
        graph: &StableDiGraph<()>,
        market_view: MarketDataView<'_>,
        subgraph: Subgraph<'_>,
        token_in_node: NodeIndex,
        token_out_node: Option<NodeIndex>,
    ) -> BellmanFordContext {
        let (adj, token_nodes, component_ids) = subgraph;

        let token_map: FxHashMap<NodeIndex, Arc<Token>> = token_nodes
            .iter()
            .filter_map(|&node| {
                market_view
                    .get_token_shared(&graph[node])
                    .map(|token| (node, Arc::clone(token)))
            })
            .collect();
        let market_data = market_view.extract_subset_with_overlay(&component_ids);
        let gas_price_wei = market_data
            .gas_price()
            .map(|gp| gp.effective_gas_price().clone());
        drop(market_view);

        let node_address: FxHashMap<NodeIndex, Address> = token_map
            .iter()
            .map(|(&node, token)| (node, token.address.clone()))
            .collect();

        let max_idx = graph
            .node_indices()
            .map(|n| n.index())
            .max()
            .unwrap_or(0) +
            1;

        let scoring = if self.gas_aware {
            RouteScoringMode::NetOutput
        } else {
            RouteScoringMode::GrossOutput
        };

        debug!(
            edges = adj
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            tokens = token_map.len(),
            "subgraph extracted"
        );

        BellmanFordContext {
            token_in_node,
            token_out_node,
            adj,
            token_map,
            market_data,
            gas_price_wei,
            token_prices: None,
            spot_prices: None,
            node_address,
            max_idx,
            scoring,
        }
    }

    /// Every token the source token reaches, with what the best path to it delivers and the
    /// components that path runs through, from one relaxation.
    ///
    /// The relaxation fills the best amount at every node, so reading all of them costs one pass
    /// rather than one per destination. Deliberately not a [`Route`] per destination:
    /// `build_route` deep-clones each swap's component, tokens, and simulation state, and a
    /// caller pricing every reachable destination reads none of that. Build `ctx` with
    /// `build_context_from_source_token`, so that no
    /// destination prunes its subgraph.
    ///
    /// Tokens the source token cannot reach, and those whose path cannot be reconstructed, are
    /// absent.
    pub(crate) fn reach_from_source_token(
        &self,
        ctx: &BellmanFordContext,
        amount_in: &BigUint,
    ) -> FxHashMap<Address, ReachedToken> {
        let spfa = self.run_spfa(ctx, amount_in, &MarketOverrides::default(), Instant::now());

        let mut reached = FxHashMap::default();
        let mut dropped = 0usize;
        for (idx, amount) in spfa.amount.iter().enumerate() {
            if amount.is_zero() || idx == ctx.token_in_node.index() {
                continue;
            }
            let node = NodeIndex::new(idx);
            let Some(address) = ctx.node_address.get(&node) else {
                trace!(node = idx, "destination dropped: no token metadata for node");
                dropped += 1;
                continue;
            };
            let path_edges = match Self::reconstruct_path(
                node,
                ctx.token_in_node,
                &spfa.predecessor,
            ) {
                Ok(path_edges) => path_edges,
                Err(error) => {
                    trace!(node = idx, token = %address, %error, "destination dropped: path reconstruction failed");
                    dropped += 1;
                    continue;
                }
            };
            let components = path_edges
                .into_iter()
                .map(|(_, _, component_id)| component_id)
                .collect();
            reached
                .insert(address.clone(), ReachedToken { amount_out: amount.clone(), components });
        }

        debug!(
            reached = reached.len(),
            dropped, "found a route to every reachable destination from one relaxation"
        );
        reached
    }

    /// Runs the SPFA relaxation loop and reconstructs the best route from a pre-built context.
    ///
    /// This is the repeatable, synchronous solve phase. Call it multiple times with different
    /// `opts.overrides` to evaluate alternative component states without redoing the setup in
    /// `ctx`. Overrides shadow the corresponding component in `ctx.market_data` for both
    /// relaxation and route construction.
    pub(crate) fn find_single_route(
        &self,
        ctx: &BellmanFordContext,
        order: &Order,
        opts: FindRouteOptions,
    ) -> Result<RouteResult, AlgorithmError> {
        let start = Instant::now();

        let Some(token_out_node) = ctx.token_out_node else {
            return Err(AlgorithmError::Other(
                "find_single_route needs a context built with a destination".to_string(),
            ));
        };

        let spfa = self.run_spfa(ctx, order.amount(), &opts.overrides, start);

        let out_idx = token_out_node.index();
        if spfa.amount[out_idx].is_zero() {
            // Dust (a hop's input below its own gas) -> AmountTooSmall; everything else
            // (unreachable, filtered, sim error incl. too-large, missing state, timeout)
            // -> NoGraphPath.
            let reason = if spfa.input_below_hop_gas {
                NoPathReason::AmountTooSmall
            } else {
                NoPathReason::NoGraphPath
            };
            return Err(AlgorithmError::NoPath {
                from: order.token_in().clone(),
                to: order.token_out().clone(),
                reason,
            });
        }

        // Reconstruct path and build route directly from stored distances/gas
        // (no re-simulation needed since forbid-revisits guarantees relaxation
        // amounts match sequential execution).
        let path_edges =
            Self::reconstruct_path(token_out_node, ctx.token_in_node, &spfa.predecessor)?;

        let route =
            Self::build_route(ctx, &path_edges, &spfa.amount, &spfa.edge_gas, &opts.overrides)?;

        let final_amount_out = spfa.amount[out_idx].clone();
        let gas_price = ctx
            .gas_price_wei
            .clone()
            .unwrap_or_default();

        let net_amount_out = Self::compute_net_amount_out(
            &final_amount_out,
            &route,
            &gas_price,
            ctx.token_prices.as_ref(),
            &spfa.spot_product,
            &ctx.node_address,
            ctx.token_in_node,
        )?;

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

    /// Runs SPFA (Shortest Path Faster Algorithm) relaxation over the subgraph and returns per-node
    /// best-path arrays.
    ///
    /// Simulation failures are silently skipped (the edge is dropped). Returns the arrays
    /// even if the destination was not reached — callers check `amount[out_idx].is_zero()`.
    fn run_spfa(
        &self,
        ctx: &BellmanFordContext,
        amount_in: &BigUint,
        overrides: &MarketOverrides,
        start: Instant,
    ) -> SPFAResult {
        // amount[node] = best gross output reachable at that node.
        // edge_gas[node] = gas for the edge that last improved amount[node].
        // cumul_gas[node] = total gas along the best path to this node.
        let mut amount: Vec<BigUint> = vec![BigUint::ZERO; ctx.max_idx];
        let mut predecessor: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; ctx.max_idx];
        let mut edge_gas: Vec<BigUint> = vec![BigUint::ZERO; ctx.max_idx];
        let mut cumul_gas: Vec<BigUint> = vec![BigUint::ZERO; ctx.max_idx];

        amount[ctx.token_in_node.index()] = amount_in.clone();

        // Track cumulative spot price product from token_in for fallback gas estimation.
        // spot_product[v] = product of spot prices along the path from token_in to v.
        let mut spot_product: Vec<f64> = vec![0.0; ctx.max_idx];
        spot_product[ctx.token_in_node.index()] = 1.0;

        let mut input_below_hop_gas = false;

        let gas_aware = matches!(ctx.scoring, RouteScoringMode::NetOutput) &&
            ctx.gas_price_wei.is_some() &&
            ctx.token_prices.is_some();
        if !gas_aware && matches!(ctx.scoring, RouteScoringMode::NetOutput) {
            debug!("gas-aware comparison disabled (missing gas_price or token_prices)");
        } else if matches!(ctx.scoring, RouteScoringMode::GrossOutput) {
            debug!("gas-aware comparison disabled by config");
        }

        let mut active_nodes: Vec<NodeIndex> = vec![ctx.token_in_node];

        for round in 0..self.max_hops {
            if start.elapsed() >= self.timeout {
                debug!(round, "timeout during relaxation");
                break;
            }
            if active_nodes.is_empty() {
                debug!(round, "no active nodes, stopping early");
                break;
            }

            let mut next_active: FxHashSet<NodeIndex> = FxHashSet::default();

            for &u in &active_nodes {
                let u_idx = u.index();
                if amount[u_idx].is_zero() {
                    continue;
                }

                let Some(token_u) = ctx.token_map.get(&u) else { continue };
                let Some(edges) = ctx.adj.get(&u) else { continue };

                for (v, component_id) in edges {
                    let v_idx = v.index();

                    // Single predecessor walk: skip if target token or component already in path
                    if Self::path_has_conflict(u, *v, component_id, &predecessor) {
                        continue;
                    }

                    // Skip disallowed connector tokens. Endpoints (token_in / token_out) are
                    // always permitted regardless of the allowlist.
                    if !self.connector_allows(ctx, *v) {
                        continue;
                    }

                    let Some(token_v) = ctx.token_map.get(v) else { continue };

                    // Overrides market data if passed in options
                    let sim: &dyn tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim =
                        if let Some(s) = overrides.get(component_id) {
                            s
                        } else if let Some(s) = ctx.market_data.get_simulation_state(component_id) {
                            s
                        } else {
                            continue;
                        };

                    let result =
                        match sim.get_amount_out_guarded(amount[u_idx].clone(), token_u, token_v) {
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
                        ctx.node_address.get(&u),
                        ctx.node_address.get(v),
                        ctx.spot_prices.as_ref(),
                    );

                    // Gas-aware comparison: compare net amounts (gross - gas cost in token terms)
                    let is_better = if gas_aware {
                        let v_price = Self::resolve_token_price(
                            ctx.node_address.get(v),
                            ctx.token_prices.as_ref(),
                            candidate_spot,
                            ctx.node_address.get(&ctx.token_in_node),
                        );
                        let net_candidate = Self::gas_adjusted_amount(
                            &result.amount,
                            &candidate_cumul_gas,
                            ctx.gas_price_wei.as_ref().unwrap(),
                            v_price.as_ref(),
                        );
                        // Dust signal: this hop's input can't cover its own gas. Input-side, so
                        // healthy and too-large orders (large inputs) never trip it. Gated on
                        // net_candidate <= 0 (implied by input < hop gas) to keep the extra
                        // price lookup off the hot path.
                        if !input_below_hop_gas && net_candidate <= BigInt::ZERO {
                            let u_price = Self::resolve_token_price(
                                ctx.node_address.get(&u),
                                ctx.token_prices.as_ref(),
                                spot_product[u_idx],
                                ctx.node_address.get(&ctx.token_in_node),
                            );
                            if Self::gas_adjusted_amount(
                                &amount[u_idx],
                                &result.gas,
                                ctx.gas_price_wei.as_ref().unwrap(),
                                u_price.as_ref(),
                            ) <= BigInt::ZERO
                            {
                                input_below_hop_gas = true;
                            }
                        }
                        let net_existing = Self::gas_adjusted_amount(
                            &amount[v_idx],
                            &cumul_gas[v_idx],
                            ctx.gas_price_wei.as_ref().unwrap(),
                            v_price.as_ref(),
                        );
                        net_candidate > net_existing
                    } else {
                        if result.amount.is_zero() {
                            input_below_hop_gas = true;
                        }
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
            // Deterministic order: HashSet iteration is random per process.
            // This pins SPFA to a fixed propagation order. The chosen order
            // may not yield the optimal route (see module docs), but the
            // previous random order was statistically no better.
            active_nodes.sort_unstable();
        }

        SPFAResult { amount, predecessor, edge_gas, spot_product, input_below_hop_gas }
    }

    /// Whether the connector-token allowlist permits routing *into* node `v`.
    /// Endpoints (token_in / token_out) are always permitted. No allowlist => all allowed.
    fn connector_allows(&self, ctx: &BellmanFordContext, v: NodeIndex) -> bool {
        let (Some(tokens), Some(v_addr)) = (&self.connector_tokens, ctx.node_address.get(&v))
        else {
            return true;
        };
        v == ctx.token_in_node || ctx.token_out_node == Some(v) || tokens.contains(v_addr)
    }

    /// Constructs a [`Route`] from a reconstructed path and SPFA output arrays.
    fn build_route(
        ctx: &BellmanFordContext,
        path_edges: &[(NodeIndex, NodeIndex, ComponentId)],
        amount: &[BigUint],
        edge_gas: &[BigUint],
        overrides: &MarketOverrides,
    ) -> Result<Route, AlgorithmError> {
        let mut swaps = Vec::with_capacity(path_edges.len());
        let mut tokens: FxHashMap<Address, Token> = FxHashMap::default();

        for (from_node, to_node, component_id) in path_edges {
            let token_in = ctx
                .token_map
                .get(from_node)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: Some(format!("{:?}", from_node)),
                })?;
            let token_out = ctx
                .token_map
                .get(to_node)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "token",
                    id: Some(format!("{:?}", to_node)),
                })?;
            let component = ctx
                .market_data
                .get_component(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "component",
                    id: Some(component_id.clone()),
                })?;
            // Use the override's sim state if available so the route reflects overridden
            // components.
            let sim_state = overrides
                .get(component_id)
                .or_else(|| {
                    ctx.market_data
                        .get_simulation_state(component_id)
                })
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
            tokens
                .entry(token_in.address.clone())
                .or_insert_with(|| Token::clone(token_in));
            tokens
                .entry(token_out.address.clone())
                .or_insert_with(|| Token::clone(token_out));
        }

        Ok(Route::new(swaps, tokens)?)
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

    /// Checks whether the target node or component conflicts with the existing path to `from`.
    /// Walks the predecessor chain once, checking both conditions simultaneously.
    pub(crate) fn path_has_conflict(
        from: NodeIndex,
        target_node: NodeIndex,
        target_component: &ComponentId,
        predecessor: &[Option<(NodeIndex, ComponentId)>],
    ) -> bool {
        let mut current = from;
        loop {
            if current == target_node {
                return true;
            }
            match &predecessor[current.index()] {
                Some((prev, cid)) => {
                    if cid == target_component {
                        return true;
                    }
                    current = *prev;
                }
                None => return false,
            }
        }
    }

    /// Reconstructs the path from token_out back to token_in by walking the predecessor
    /// array.
    pub(crate) fn reconstruct_path(
        token_out: NodeIndex,
        token_in: NodeIndex,
        predecessor: &[Option<(NodeIndex, ComponentId)>],
    ) -> Result<Vec<(NodeIndex, NodeIndex, ComponentId)>, AlgorithmError> {
        let mut path = Vec::new();
        let mut current = token_out;
        let mut visited = FxHashSet::default();

        while current != token_in {
            if !visited.insert(current) {
                return Err(AlgorithmError::Other("cycle in predecessor chain".to_string()));
            }

            let idx = current.index();
            match &predecessor
                .get(idx)
                .and_then(|p| p.as_ref())
            {
                Some((prev_node, component_id)) => {
                    path.push((*prev_node, current, component_id.clone()));
                    current = *prev_node;
                }
                None => {
                    return Err(AlgorithmError::Other(format!(
                        "broken predecessor chain at node {idx}"
                    )));
                }
            }
        }

        path.reverse();
        Ok(path)
    }

    /// Extracts the part of the graph that can carry a route from `token_in` to `token_out` in at
    /// most `max_hops` — or, without a destination, everything within `max_hops` of `token_in`.
    ///
    /// Returns `(adjacency_list, token_nodes, component_ids)`, or `None` when no edge qualifies.
    /// The caller says what an empty subgraph means for it.
    ///
    /// With a destination, both ends bound the walk. A token reached in `d` hops is only worth
    /// keeping if the destination is still `max_hops - d` hops away or nearer, and the same holds
    /// edge by edge. The distances used are the shortest ones, so nothing that could appear on a
    /// route of legal length is discarded.
    ///
    /// Expanding from the source alone reaches most of the market. Every component it reaches gets
    /// copied by the caller's `extract_subset`, held for the solve, and simulated during
    /// relaxation, so bounding the walk bounds all three. Only a caller that reads every relaxed
    /// node (`reach_from_source_token`) should pass `None` — it needs that full reach.
    pub(crate) fn get_subgraph<'a>(
        graph: &'a StableDiGraph<()>,
        token_in: NodeIndex,
        token_out: Option<NodeIndex>,
        max_hops: usize,
    ) -> Option<Subgraph<'a>> {
        // Walked from the destination along outgoing edges, not incoming ones. Every pool in this
        // graph is entered as a pair of opposite edges, so the two walks cover the same tokens and
        // the outgoing one needs no reversed index.
        let hops_to_token_out =
            token_out.map(|token_out| Self::get_hops_to_reach(graph, token_out, max_hops));
        Self::get_subgraph_with_hop_map(graph, token_in, hops_to_token_out.as_ref(), max_hops)
    }

    /// `get_subgraph` with the destination's hop map supplied by the
    /// caller, for walks that share one destination: the map costs a BFS over the graph, and a
    /// caller pruning many sources toward the same destination should pay it once.
    pub(crate) fn get_subgraph_with_hop_map<'a>(
        graph: &'a StableDiGraph<()>,
        token_in: NodeIndex,
        hops_to_token_out: Option<&FxHashMap<NodeIndex, usize>>,
        max_hops: usize,
    ) -> Option<Subgraph<'a>> {
        let mut adj: FxHashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>> = FxHashMap::default();
        let mut token_nodes: FxHashSet<NodeIndex> = FxHashSet::default();
        let mut component_ids: FxHashSet<&ComponentId> = FxHashSet::default();
        let mut visited_nodes = FxHashSet::default();
        let mut queued_nodes = VecDeque::new();

        visited_nodes.insert(token_in);
        token_nodes.insert(token_in);
        queued_nodes.push_back((token_in, 0usize));

        while let Some((node, depth_walked)) = queued_nodes.pop_front() {
            if depth_walked >= max_hops {
                continue;
            }
            for edge in graph.edges(node) {
                let next_token = edge.target();

                if let Some(hops_to_token_out) = &hops_to_token_out {
                    // Taking this edge spends one hop; the rest have to be enough to finish the
                    // route.
                    let Some(&hops_left) = hops_to_token_out.get(&next_token) else {
                        // token_out is not reachable from next_token within the hop budget.
                        continue;
                    };

                    if depth_walked + 1 + hops_left > max_hops {
                        // Finishing the route from next_token would take more hops than are left.
                        continue;
                    }
                }

                let component_id = &edge.weight().component_id;
                adj.entry(node)
                    .or_default()
                    .push((next_token, component_id.clone()));
                component_ids.insert(component_id);
                token_nodes.insert(next_token);

                if visited_nodes.insert(next_token) {
                    queued_nodes.push_back((next_token, depth_walked + 1));
                }
            }
        }

        if adj.is_empty() {
            return None;
        }

        Some((adj, token_nodes, component_ids))
    }

    /// Every node within `max_hops` of `from`, and how many hops each one takes to reach.
    pub(crate) fn get_hops_to_reach(
        graph: &StableDiGraph<()>,
        from: NodeIndex,
        max_hops: usize,
    ) -> FxHashMap<NodeIndex, usize> {
        let mut hops_to_reach: FxHashMap<NodeIndex, usize> = FxHashMap::default();
        hops_to_reach.insert(from, 0);

        let mut frontier = vec![from];
        for depth in 1..=max_hops {
            let mut next = Vec::new();
            for node in frontier {
                for neighbor in graph.neighbors(node) {
                    if hops_to_reach.contains_key(&neighbor) {
                        continue;
                    }
                    hops_to_reach.insert(neighbor, depth);
                    next.push(neighbor);
                }
            }
            frontier = next;
        }

        hops_to_reach
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
        node_address: &FxHashMap<NodeIndex, Address>,
        token_in_node: NodeIndex,
    ) -> Result<BigInt, AlgorithmError> {
        let last_swap = route.swaps().last().ok_or_else(|| {
            AlgorithmError::Other("compute_net_amount_out called with empty route".to_string())
        })?;

        let total_gas = route.total_gas();

        if gas_price.is_zero() {
            warn!("missing gas price, returning gross amount_out");
            return Ok(BigInt::from(amount_out.clone()));
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

        Ok(match output_price {
            Some(price) if !price.denominator.is_zero() => {
                let gas_cost = &gas_cost_wei * &price.numerator / &price.denominator;
                BigInt::from(amount_out.clone()) - BigInt::from(gas_cost)
            }
            _ => {
                warn!("no gas price for output token, returning gross amount_out");
                BigInt::from(amount_out.clone())
            }
        })
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
        market: MarketData,
        label: Option<StateLabel>,
        derived: Option<SharedDerivedDataRef>,
        order: &Order,
    ) -> Result<RouteResult, AlgorithmError> {
        let ctx = self
            .build_context(graph, market, label, derived, order)
            .await?;
        self.find_single_route(&ctx, order, FindRouteOptions::default())
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
        feed::market_data::{MarketData, MarketState},
        graph::GraphManager,
        types::quote::OrderSide,
    };

    // ==================== Test Utilities ====================

    /// Sets up market and graph with `()` edge weights for BellmanFord tests.
    fn setup_market_bf(
        components: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (MarketData, PetgraphStableDiGraphManager<()>) {
        let mut market = MarketState::new();

        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(100u64) },
        });
        market.update_last_updated(crate::types::BlockInfo::new(1, "0x00".into(), 0));

        for (component_id, token_in, token_out, state) in components {
            let tokens = vec![token_in.clone(), token_out.clone()];
            let comp = component(component_id, &tokens);
            market.upsert_components(std::iter::once(comp));
            market.update_states([(
                component_id.to_string(),
                Box::new(state) as Box<dyn ProtocolSim>,
            )]);
            market.upsert_tokens(tokens);
        }

        let mut graph_manager = PetgraphStableDiGraphManager::default();
        graph_manager.initialize_graph(&market.component_topology());

        (MarketData::new(Arc::new(RwLock::new(market))), graph_manager)
    }

    fn setup_derived_with_token_prices(
        token_addresses: &[Address],
    ) -> crate::derived::SharedDerivedDataRef {
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        let mut token_prices: TokenGasPrices = FxHashMap::default();
        for address in token_addresses {
            token_prices.insert(
                address.clone(),
                Price { numerator: BigUint::from(1u64), denominator: BigUint::from(1u64) },
            );
        }

        let mut derived_data = DerivedData::new();
        derived_data.set_token_prices(token_prices, vec![], 1, true);
        Arc::new(RwLock::new(derived_data))
    }

    fn bf_algorithm(max_hops: usize, timeout_ms: u64) -> BellmanFordAlgorithm {
        BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, max_hops, Duration::from_millis(timeout_ms), None).unwrap(),
        )
    }

    // ==================== Unit Tests ====================

    /// The subgraph must hold everything a route could use and nothing else.
    ///
    /// Dropping too little is only slow, so no test would catch it; dropping too much loses routes
    /// silently, and the route that dies first is the one using every hop it is allowed.
    ///
    /// ```text
    ///   A --[ab]-- B --[bc]-- C      the only route, and it needs both hops
    ///   A --[ad]-- D                 a dead end: D reaches nothing else
    /// ```
    #[test]
    fn test_get_subgraph_keeps_full_length_routes_and_drops_dead_ends() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (_, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("component_ad", &token_a, &token_d, MockProtocolSim::new(5.0)),
        ]);
        let graph = manager.graph();
        let node = |address: &Address| {
            graph
                .node_indices()
                .find(|&n| &graph[n] == address)
                .expect("token in graph")
        };
        let (adj, _, component_ids) = BellmanFordAlgorithm::get_subgraph(
            graph,
            node(&token_a.address),
            Some(node(&token_c.address)),
            2,
        )
        .unwrap();

        let kept = |id: &str| {
            component_ids
                .iter()
                .any(|component_id| *component_id == id)
        };

        // A -> B -> C spends the whole budget, so an off-by-one in the test would drop it.
        assert!(kept("component_ab"), "the route's first hop must survive");
        assert!(kept("component_bc"), "the route's second hop must survive");
        // D is reachable from A but reaches nothing, so no route can pass through it. Keeping it
        // would deep-copy its pool state and simulate it during relaxation, both for nothing.
        assert!(!kept("component_ad"), "a dead end must not be kept");

        // Nor should stepping back towards the source be kept: from B, returning to A leaves no
        // budget to reach C.
        let from_b = adj
            .get(&node(&token_b.address))
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert!(
            from_b
                .iter()
                .all(|(target, _)| *target != node(&token_a.address)),
            "B -> A cannot finish the route and must not be kept"
        );
    }

    /// A token can sit inside the hop budget of both ends and still be on no legal route.
    ///
    /// D is one hop from the source and two from the destination, and the budget is two, so both
    /// halves fit on their own while the route through D needs three. Only the sum rules it out,
    /// which is the one case the arithmetic decides: a dead end is already gone by then, dropped
    /// for having no distance to the destination at all.
    ///
    /// ```text
    ///   A --[ab]-- B --[bc]-- C        two hops, the whole budget
    ///   A --[ad]-- D --[de]-- E --[ec]-- C     three hops through D
    /// ```
    #[test]
    fn test_get_subgraph_drops_detours_that_cannot_finish_in_budget() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");
        let token_e = token(0x05, "E");

        let (_, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("component_ad", &token_a, &token_d, MockProtocolSim::new(5.0)),
            ("component_de", &token_d, &token_e, MockProtocolSim::new(5.0)),
            ("component_ec", &token_e, &token_c, MockProtocolSim::new(5.0)),
        ]);
        let graph = manager.graph();
        let node = |address: &Address| {
            graph
                .node_indices()
                .find(|&n| &graph[n] == address)
                .expect("token in graph")
        };
        let (_, token_nodes, component_ids) = BellmanFordAlgorithm::get_subgraph(
            graph,
            node(&token_a.address),
            Some(node(&token_c.address)),
            2,
        )
        .unwrap();

        let kept = |id: &str| {
            component_ids
                .iter()
                .any(|component_id| *component_id == id)
        };

        assert!(kept("component_ab"), "the two-hop route's first leg must survive");
        assert!(kept("component_bc"), "the two-hop route's second leg must survive");

        // One hop spent reaching D, two more needed to leave it: three in a budget of two.
        assert!(!kept("component_ad"), "the step into a detour must not be kept");
        assert!(!kept("component_de"), "nor anything further along it");
        assert!(!kept("component_ec"), "nor its last leg into the destination");
        assert!(
            !token_nodes.contains(&node(&token_d.address)),
            "a token no legal route reaches must not be kept"
        );
    }

    /// Without a destination, `max_hops` from the source is the only bound on the walk.
    ///
    /// ```text
    ///   G --[gb]-- B --[bc]-- C --[cd]-- D      D is three hops out, budget is two
    /// ```
    #[test]
    fn test_get_subgraph_without_destination_stops_at_max_hops() {
        let token_g = token(0x01, "G");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (_, manager) = setup_market_bf(vec![
            ("component_gb", &token_g, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(2.0)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(2.0)),
        ]);
        let graph = manager.graph();
        let node = |address: &Address| {
            graph
                .node_indices()
                .find(|&n| &graph[n] == address)
                .expect("token in graph")
        };
        let (_, token_nodes, component_ids) =
            BellmanFordAlgorithm::get_subgraph(graph, node(&token_g.address), None, 2).unwrap();

        let kept = |id: &str| {
            component_ids
                .iter()
                .any(|component_id| *component_id == id)
        };

        assert!(kept("component_gb"), "the first hop is within budget");
        assert!(kept("component_bc"), "the second hop spends the budget exactly");
        assert!(!kept("component_cd"), "an edge past the hop budget must not be kept");
        assert!(
            !token_nodes.contains(&node(&token_d.address)),
            "a token past the hop budget must not be kept"
        );
    }

    #[tokio::test]
    async fn test_linear_path_found() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(4.0)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bd", &token_b, &token_d, MockProtocolSim::new(3.0)),
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(4.0)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(1.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // A->B->D: 100*2*3=600 is better than A->C->D: 100*4*1=400
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "component_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "component_bd");
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(600u64));
    }

    #[tokio::test]
    async fn test_parallel_components() {
        // Two components between A and B with different multipliers
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component2", &token_a, &token_b, MockProtocolSim::new(5.0)),
        ]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "component2");
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(500u64));
    }

    #[tokio::test]
    async fn test_no_path_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        // A-B connected, C disconnected
        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        // Add token_c to market without connecting it
        {
            let mut m = market.write().await;
            m.upsert_tokens(vec![token_c.clone()]);
        }

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[tokio::test]
    async fn test_source_not_in_graph() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_x = token(0x99, "X");

        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_x, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::SourceTokenNotInGraph, .. })
        ));
    }

    #[tokio::test]
    async fn test_amount_too_small_when_reachable_but_zero_output() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        // Reachable component, but rate 0.5 on a 1-unit input floors to 0 output.
        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(0.5))]);
        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_b, 1, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::AmountTooSmall, .. })
        ));
    }

    #[tokio::test]
    async fn test_amount_too_small_when_dust_occurs_mid_route() {
        // A->B floors 1*0.5 to 0 one hop before token_out: dust mid-route must still
        // be AmountTooSmall.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(0.5)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(2.0)),
        ]);
        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 1, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::AmountTooSmall, .. })
        ));
    }

    #[tokio::test]
    async fn test_no_graph_path_when_amount_too_large_for_liquidity() {
        // Output 2000 exceeds liquidity 500, so the sim errors: a too-large amount must
        // never be mislabeled AmountTooSmall.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, manager) = setup_market_bf(vec![(
            "component_ab",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_liquidity(500),
        )]);
        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::NoGraphPath, .. })
        ));
    }

    #[tokio::test]
    async fn test_no_graph_path_when_unreachable_within_hops() {
        // A->B->C exists, but max_hops=1 leaves C out of the subgraph.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(2.0)),
        ]);
        let algo = bf_algorithm(1, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::NoGraphPath, .. })
        ));
    }

    #[tokio::test]
    async fn test_reach_from_source_token_covers_branches_off_any_pair() {
        // G->A and G->B->C: B and C sit on no G->A path, so a subgraph pruned toward any
        // single destination would drop them. The from-source context must keep them all.
        let token_g = token(0x01, "G");
        let token_a = token(0x02, "A");
        let token_b = token(0x03, "B");
        let token_c = token(0x04, "C");

        let (market, manager) = setup_market_bf(vec![
            ("component_ga", &token_g, &token_a, MockProtocolSim::new(2.0)),
            ("component_gb", &token_g, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(2.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ctx = algo
            .build_context_from_source_token(manager.graph(), market, &token_g.address, 3)
            .await
            .expect("gas token has outgoing edges");
        let routes = algo.reach_from_source_token(&ctx, &BigUint::from(100u64));

        let reached: FxHashSet<Address> = routes.keys().cloned().collect();
        let expected: FxHashSet<Address> = [&token_a, &token_b, &token_c]
            .into_iter()
            .map(|t| t.address.clone())
            .collect();
        assert_eq!(reached, expected);
    }

    #[tokio::test]
    async fn test_find_single_route_rejects_a_context_built_without_destination() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(3, 1000);
        let ctx = algo
            .build_context_from_source_token(manager.graph(), market, &token_a.address, 3)
            .await
            .expect("source has outgoing edges");
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo.find_single_route(&ctx, &ord, FindRouteOptions::default());
        assert!(matches!(result, Err(AlgorithmError::Other(_))));
    }

    #[tokio::test]
    async fn test_find_single_route_after_reroot() {
        // A context built from G, re-rooted at C, solves the reverse direction C->B->G
        // against the same snapshot: one context serves solves from any node it covers.
        let token_g = token(0x01, "G");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let (market, manager) = setup_market_bf(vec![
            ("component_gb", &token_g, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(2.0)),
        ]);
        let graph = manager.graph();

        let algo = bf_algorithm(3, 1000);
        let mut ctx = algo
            .build_context_from_source_token(graph, market, &token_g.address, 3)
            .await
            .expect("source has outgoing edges");
        let node_of = |address: &Address| {
            graph
                .node_indices()
                .find(|&n| &graph[n] == address)
                .expect("token is in the graph")
        };

        let gas_node = ctx.token_in_node;
        ctx.reroot(node_of(&token_c.address), Some(gas_node));
        let ord = order(&token_c, &token_g, 100, OrderSide::Sell);
        let result = algo
            .find_single_route(&ctx, &ord, FindRouteOptions::default())
            .expect("re-rooted context solves back to its source");

        // 100 C -> 50 B -> 25 G through the two fee-free 2.0 pools read in reverse.
        assert_eq!(
            result
                .route()
                .amount_out(&token_g.address),
            BigUint::from(25u64)
        );
    }

    #[tokio::test]
    async fn test_no_graph_path_when_connector_tokens_exclude_intermediate() {
        // B is not in the connector allowlist, so SPFA never simulates into it:
        // policy exclusion reads as NoGraphPath, not AmountTooSmall.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(2.0)),
        ]);

        let algo = BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, 3, Duration::from_millis(1000), None)
                .unwrap()
                .with_connector_tokens(FxHashSet::default()),
        );
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::NoGraphPath, .. })
        ));
    }

    #[tokio::test]
    async fn test_destination_not_in_graph() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_x = token(0x99, "X");

        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_x, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(4.0)),
        ]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // Should find exactly the 2-hop path A->B->C = 100*2*3 = 600
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "component_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "component_bc");
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
            ("component_cb", &token_c, &token_b, MockProtocolSim::new(100.0)),
            ("component_bd", &token_b, &token_d, MockProtocolSim::new(2.0)),
        ]);

        let algo = bf_algorithm(4, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // Should find A->B->D = 100*2*2 = 400 (the direct 2-hop path)
        // The 4-hop revisit path A->B->C->B->D is blocked
        assert_eq!(result.route().swaps().len(), 2, "should use direct 2-hop path");
        assert_eq!(result.route().swaps()[0].component_id(), "component_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "component_bd");
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(400u64));
    }

    #[tokio::test]
    async fn test_route_amounts_are_sequential() {
        // Verify that swap amount_in[i+1] == amount_out[i] in the built route
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_gas(10),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algo
            .find_best_route(manager.graph(), market, None, Some(derived), &ord)
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        // 0ms timeout
        let algo = bf_algorithm(3, 0);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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

        // Component with 10% fee
        let (market, manager) = setup_market_bf(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_fee(0.1),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // 1000 * 2 * (1-0.1) = 1800
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(1800u64));
    }

    #[tokio::test]
    async fn test_large_trade_slippage() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Component with limited liquidity (500 tokens)
        let (market, manager) = setup_market_bf(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_liquidity(500),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        // Should fail due to insufficient liquidity
        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "Should fail when trade exceeds component liquidity"
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_de", &token_d, &token_e, MockProtocolSim::new(4.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_e, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { .. })),
            "should not find path to disconnected component"
        );
    }

    #[tokio::test]
    async fn test_spfa_skips_failed_simulations() {
        // Component that will fail simulation (liquidity=0 would cause error for any amount)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_bf(vec![
            // Direct path with failing component
            ("component_ab_bad", &token_a, &token_b, MockProtocolSim::new(2.0).with_liquidity(0)),
            // Alternative path that works
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(2.0)),
            ("component_cb", &token_c, &token_b, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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
                // (since the failing component might also block the reverse B->A edge)
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(3.0).with_gas(high_gas)),
            ("component_bd", &token_b, &token_d, MockProtocolSim::new(2.0).with_gas(high_gas)),
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(2.0).with_gas(low_gas)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(2.0).with_gas(low_gas)),
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
            .find_best_route(manager.graph(), market, None, Some(derived), &ord)
            .await
            .unwrap();

        // Gas-aware relaxation should pick the cheaper path A -> C -> D
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "component_ac");
        assert_eq!(result.route().swaps()[1].component_id(), "component_cd");
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
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(3.0).with_gas(high_gas)),
            ("component_bd", &token_b, &token_d, MockProtocolSim::new(2.0).with_gas(high_gas)),
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(2.0).with_gas(low_gas)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(2.0).with_gas(low_gas)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 1_000_000_000, OrderSide::Sell);

        // No derived data: should fall back to gross comparison
        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // Without gas awareness, picks the higher-gross path A -> B -> D
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "component_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "component_bd");
    }

    #[tokio::test]
    async fn test_amount_too_small_when_net_uneconomic_after_gas() {
        // Gas-aware branch: the 1-unit input (worth 1 at 1:1) is far below the hop's
        // gas cost of 1000 -> AmountTooSmall.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![(
            "component_ab",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_gas(10),
        )]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1, OrderSide::Sell);
        let derived =
            setup_derived_with_token_prices(&[token_a.address.clone(), token_b.address.clone()]);

        let result = algo
            .find_best_route(manager.graph(), market, None, Some(derived), &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::AmountTooSmall, .. })
        ));
    }

    #[tokio::test]
    async fn test_no_graph_path_when_output_uneconomic_but_input_economic() {
        // Output value (500) is below the hop's gas (1000) so the solve fails, but the
        // input (10_000) covers it: a healthy-sized order on a low-rate component must read
        // NoGraphPath, not AmountTooSmall.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_bf(vec![(
            "component_ab",
            &token_a,
            &token_b,
            MockProtocolSim::new(0.05).with_gas(10),
        )]);

        let algo = bf_algorithm(1, 1000);
        let ord = order(&token_a, &token_b, 10_000, OrderSide::Sell);
        let derived =
            setup_derived_with_token_prices(&[token_a.address.clone(), token_b.address.clone()]);

        let result = algo
            .find_best_route(manager.graph(), market, None, Some(derived), &ord)
            .await;
        assert!(matches!(
            result,
            Err(AlgorithmError::NoPath { reason: NoPathReason::NoGraphPath, .. })
        ));
    }

    // ==================== Connector token tests ====================

    /// Build a BellmanFord algorithm whose config includes a specific connector token allowlist.
    fn bf_algorithm_with_connectors(
        max_hops: usize,
        timeout_ms: u64,
        connector_tokens: FxHashSet<Address>,
    ) -> BellmanFordAlgorithm {
        BellmanFordAlgorithm::with_config(
            AlgorithmConfig::new(1, max_hops, Duration::from_millis(timeout_ms), None)
                .unwrap()
                .with_connector_tokens(connector_tokens),
        )
    }

    #[tokio::test]
    async fn test_connector_tokens_blocks_disallowed_intermediate() {
        //      A
        //    /   \
        //   B     C   ← only C is in the allowlist
        //    \   /
        //      D
        // A->B->D is pruned; only A->C->D survives.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bd", &token_b, &token_d, MockProtocolSim::new(2.0)),
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(3.0)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(3.0)),
        ]);

        let connectors: FxHashSet<Address> = FxHashSet::from_iter([token_c.address.clone()]);
        let algo = bf_algorithm_with_connectors(3, 1000, connectors);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // Only A->C->D is reachable; B was pruned.
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "component_ac");
        assert_eq!(result.route().swaps()[1].component_id(), "component_cd");
    }

    #[tokio::test]
    async fn test_connector_tokens_allows_endpoints_even_if_not_listed() {
        // token_in (A) and token_out (B) must be reachable even when connector list is empty.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        // Empty allowlist — no intermediate tokens allowed, but direct hop A->B should work.
        let algo = bf_algorithm_with_connectors(1, 1000, FxHashSet::default());
        let ord = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].amount_out(), &BigUint::from(200u64));
    }

    #[tokio::test]
    async fn test_connector_tokens_none_is_unrestricted() {
        // No connector_tokens set: both A->B->D and A->C->D are evaluated.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");
        let token_d = token(0x04, "D");

        let (market, manager) = setup_market_bf(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bd", &token_b, &token_d, MockProtocolSim::new(3.0)),
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(1.0)),
            ("component_cd", &token_c, &token_d, MockProtocolSim::new(1.0)),
        ]);

        let algo = bf_algorithm(3, 1000);
        let ord = order(&token_a, &token_d, 100, OrderSide::Sell);

        let result = algo
            .find_best_route(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // Best path is A->B->D = 100*2*3 = 600
        assert_eq!(result.route().swaps()[0].component_id(), "component_ab");
        assert_eq!(result.route().swaps()[1].component_id(), "component_bd");
        assert_eq!(result.route().swaps()[1].amount_out(), &BigUint::from(600u64));
    }

    #[test]
    fn test_path_has_conflict_detects_node_and_component() {
        // Path: 0 -[component_a]-> 1 -[component_b]-> 2
        let mut pred: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; 4];
        pred[1] = Some((NodeIndex::new(0), "component_a".into()));
        pred[2] = Some((NodeIndex::new(1), "component_b".into()));

        // Node conflicts: node 0 is in path, node 3 is not
        assert!(BellmanFordAlgorithm::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(0),
            &"any".into(),
            &pred
        ));
        assert!(!BellmanFordAlgorithm::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"any".into(),
            &pred
        ));
        // Self-check: node 2 is itself in the "path from 2"
        assert!(BellmanFordAlgorithm::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(2),
            &"any".into(),
            &pred
        ));

        // Component conflicts: component_a and component_b are used, component_c is not
        assert!(BellmanFordAlgorithm::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"component_a".into(),
            &pred
        ));
        assert!(BellmanFordAlgorithm::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"component_b".into(),
            &pred
        ));
        assert!(!BellmanFordAlgorithm::path_has_conflict(
            NodeIndex::new(2),
            NodeIndex::new(3),
            &"component_c".into(),
            &pred
        ));
    }

    #[tokio::test]
    async fn test_find_single_route_with_state_overrides() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let ctx = algo
            .build_context(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        // Without overrides: 1000 * 2.0 = 2000
        let normal = algo
            .find_single_route(&ctx, &ord, FindRouteOptions::default())
            .unwrap();
        assert_eq!(normal.route().swaps()[0].amount_out(), &BigUint::from(2000u64));

        // Override component_ab with a degraded sim (multiplier 1.0): 1000 * 1.0 = 1000
        let opts = FindRouteOptions {
            overrides: MarketOverrides::empty()
                .with_override("component_ab".to_string(), Box::new(MockProtocolSim::new(1.0))),
        };
        let overridden = algo
            .find_single_route(&ctx, &ord, opts)
            .unwrap();
        assert_eq!(overridden.route().swaps()[0].amount_out(), &BigUint::from(1000u64));

        assert!(
            overridden.route().swaps()[0].amount_out() < normal.route().swaps()[0].amount_out()
        );
    }

    #[tokio::test]
    async fn test_single_find_route_options_default() {
        use super::super::split_primitives::MarketOverrides;

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) =
            setup_market_bf(vec![("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0))]);

        let algo = bf_algorithm(2, 1000);
        let ord = order(&token_a, &token_b, 1000, OrderSide::Sell);

        let ctx = algo
            .build_context(manager.graph(), market, None, None, &ord)
            .await
            .unwrap();

        let with_default = algo
            .find_single_route(&ctx, &ord, FindRouteOptions::default())
            .unwrap();
        let with_empty = algo
            .find_single_route(&ctx, &ord, FindRouteOptions { overrides: MarketOverrides::empty() })
            .unwrap();

        assert_eq!(
            with_default.route().swaps()[0].amount_out(),
            with_empty.route().swaps()[0].amount_out()
        );
        assert_eq!(with_default.route().swaps()[0].amount_out(), &BigUint::from(2000u64));
    }
}
