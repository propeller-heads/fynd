//! Most Liquid algorithm implementation.
//!
//! This algorithm finds routes by:
//! 1. Finding every route between the two tokens as a sequence of tokens, no pools chosen yet
//! 2. Ranking those sequences by spot price and liquidity depth, deepest first
//! 3. Solving each sequence hop by hop, taking the pool that pays most at the amount that reaches
//!    it — one simulation per pool per hop rather than per pool combination, and never the same
//!    pool on two hops
//! 4. Ranking the solved sequences by net output (output less gas, in the output token)
//! 5. Building swaps for the winner alone, with stats recorded to the tracing span

use std::time::{Duration, Instant};

use metrics::{counter, histogram};
use num_bigint::{BigInt, BigUint};
use num_traits::ToPrimitive;
use petgraph::stable_graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use tracing::{debug, instrument, trace};
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::{GetAmountOutResult, Price, ProtocolSim},
    tycho_core::models::{token::Token, Address},
};

use super::{Algorithm, AlgorithmConfig, NoPathReason};
use crate::{
    algorithm::{paths, sim_guard::GuardedProtocolSim},
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketState, StateLabel},
    graph::{
        EdgeData, GraphQueryFilter, TokenPath, TopologyGraph, TopologyGraphManager, INLINE_EDGES,
    },
    types::{ComponentId, Order, Route, RouteResult, Swap},
    AlgorithmError,
};

/// What can go wrong settling one token sequence.
///
/// Never leaves most-liquid: the caller counts it and tries the next sequence. Variants carry node
/// indices rather than addresses, so a sequence that fails costs nothing to report.
#[derive(Debug, thiserror::Error)]
enum MostLiquidError {
    /// No pool trading this pair could take the amount that reached it.
    #[error("no pool between {from:?} and {to:?} could trade the amount")]
    HopNotTradable { from: NodeIndex, to: NodeIndex },
    /// A token on the sequence is not in the market subset this solve was given.
    #[error("token {0:?} not in the market subset")]
    TokenMissing(NodeIndex),
}

/// What every candidate on one order is solved against.
///
/// Fixed for the whole solve, so it travels as one argument rather than as five that would have to
/// stay in step.
#[derive(Clone, Copy)]
struct SolveContext<'a> {
    graph: &'a TopologyGraph<DepthAndPrice>,
    market: &'a MarketState,
    token_prices: Option<&'a TokenGasPrices>,
    amount_in: &'a BigUint,
    /// What a unit of gas costs, read once off the market snapshot.
    gas_price: &'a BigUint,
    /// When the solve started, against which every candidate checks the timeout.
    start: Instant,
}

/// A token sequence with every hop solved, before any swap is built.
struct SolvedRoute {
    hops: SmallVec<[HopResult; INLINE_EDGES]>,
    /// The route's output less the gas it costs, in the output token's own units. Falls back to
    /// the gross amount when that token has no price, which is what happens before derived data
    /// has been computed.
    net_amount_out: BigInt,
}

/// One half of a token's gas price as an `f64`, for normalising depth.
///
/// `None` when it is zero or too large for an `f64`, either of which makes the edge unusable.
/// `part` names which half, for the log.
fn price_part(
    value: &BigUint,
    part: &'static str,
    component_id: &ComponentId,
    token_in: &Token,
) -> Option<f64> {
    match value.to_f64() {
        Some(v) if v > 0.0 => Some(v),
        Some(_) => {
            trace!(
                component_id = %component_id,
                token_in = %token_in.address,
                part,
                "token price part is zero, skipping edge"
            );
            None
        }
        None => {
            trace!(
                component_id = %component_id,
                token_in = %token_in.address,
                part,
                "token price part overflows f64, skipping edge"
            );
            None
        }
    }
}

/// How one solve went: what it looked at, what it dropped, and why.
///
/// Filled in as the solve runs, then handed to [`SolveReport::record`] once, which owns every
/// metric and every log line the solve emits.
#[derive(Default)]
struct SolveReport {
    /// Token sequences the graph search returned.
    paths_candidates: usize,
    /// Of those, the ones left after ranking and the `max_routes` cut.
    paths_to_simulate: usize,
    /// Of those, the ones reached before the timeout stopped the solve.
    paths_simulated: usize,
    /// Sequences the ranking could not place at all, so they were never simulated.
    scoring_failures: usize,
    /// Sequences with a hop no pool would trade.
    simulation_failures: usize,
    /// Winners whose swaps did not form a route the executor can take.
    validation_failures: usize,
}

impl SolveReport {
    /// How much of what was worth simulating actually got simulated, as a percentage.
    fn coverage_pct(&self) -> f64 {
        (self.paths_simulated as f64 / self.paths_to_simulate as f64) * 100.0
    }

    /// Records the counters and writes the line that says how the solve went.
    fn record(
        &self,
        best: Option<&RouteResult>,
        market: &MarketState,
        amount_in: &BigUint,
        solve_time_ms: u64,
        components_considered: usize,
    ) {
        counter!("algorithm.scoring_failures").increment(self.scoring_failures as u64);
        counter!("algorithm.simulation_failures").increment(self.simulation_failures as u64);
        counter!("algorithm.validation_failures").increment(self.validation_failures as u64);
        histogram!("algorithm.simulation_coverage_pct").record(self.coverage_pct());

        let block_number = market
            .last_updated()
            .map(|b| b.number());
        let tokens_considered = market.token_registry_ref().len();
        let Some(result) = best else {
            debug!(
                solve_time_ms,
                block_number,
                paths_candidates = self.paths_candidates,
                paths_to_simulate = self.paths_to_simulate,
                paths_simulated = self.paths_simulated,
                simulation_failures = self.simulation_failures,
                validation_failures = self.validation_failures,
                simulation_coverage_pct = self.coverage_pct(),
                components_considered,
                tokens_considered,
                "no viable route"
            );
            return;
        };

        let path_desc = result
            .route()
            .path_description(market.token_registry_ref());
        let protocols = result
            .route()
            .swaps()
            .iter()
            .map(|s| s.protocol())
            .collect::<Vec<_>>();
        let price = amount_in
            .to_f64()
            .filter(|&v| v > 0.0)
            .and_then(|amt_in| {
                result
                    .net_amount_out()
                    .to_f64()
                    .map(|amt_out| amt_out / amt_in)
            })
            .unwrap_or(f64::NAN);

        debug!(
            solve_time_ms,
            block_number,
            paths_candidates = self.paths_candidates,
            paths_to_simulate = self.paths_to_simulate,
            paths_simulated = self.paths_simulated,
            simulation_failures = self.simulation_failures,
            validation_failures = self.validation_failures,
            simulation_coverage_pct = self.coverage_pct(),
            components_considered,
            tokens_considered,
            path = %path_desc,
            amount_in = %amount_in,
            net_amount_out = %result.net_amount_out(),
            price_out_per_in = price,
            hop_count = result.route().swaps().len(),
            protocols = ?protocols,
            "route found"
        );
    }
}

/// A built route's output less what its gas costs, in the output token.
///
/// Read off the swaps rather than off the solve, because the swaps are simulated against each
/// other's state: a pool the route crosses twice pays the second time what it really would, which
/// the hop-by-hop solve does not account for. Falls back to the gross amount when the output token
/// has no price, which is what happens before derived data has been computed.
fn swap_on_route(
    route: &Route,
    token_prices: Option<&TokenGasPrices>,
    gas_price: &BigUint,
) -> BigInt {
    let Some(last) = route.swaps().last() else {
        return BigInt::ZERO;
    };
    let amount_out = BigInt::from(last.amount_out().clone());

    let Some(price) = token_prices.and_then(|prices| prices.get(last.token_out())) else {
        return amount_out;
    };
    let mut gas = BigUint::ZERO;
    for swap in route.swaps() {
        gas += swap.gas_estimate();
    }

    amount_out - BigInt::from(gas * gas_price * &price.numerator / &price.denominator)
}

/// What one pool paid for one input amount, on one token pair.
///
/// Holds no pool state and no component: a candidate route is solved to compare it, and only the
/// winner is built into swaps.
#[derive(Clone)]
struct HopResult {
    /// Where the pool these numbers came from sits in the pair's pool list. An index rather than
    /// an id, so remembering one costs nothing: the list is fixed for as long as an order is being
    /// solved.
    pool_ix: usize,
    /// What that pool paid out, before gas.
    amount_out: BigUint,
    /// What that swap costs in gas, in wei.
    gas: BigUint,
}

impl HopResult {
    fn new(pool_ix: usize, result: GetAmountOutResult) -> Self {
        HopResult { pool_ix, amount_out: result.amount, gas: result.gas }
    }
}

/// Algorithm that selects routes based on expected output after gas.
pub struct MostLiquidAlgorithm {
    /// The hop bounds and connector tokens every route search runs under. Owned, so a solve hands
    /// it straight to the graph instead of assembling one per order.
    query: GraphQueryFilter,
    timeout: Duration,
    max_routes: Option<usize>,
    cache_pair_swaps: bool,
}

/// Algorithm-specific edge data for liquidity-based routing.
///
/// Used by the MostLiquid algorithm to score paths based on expected output.
/// Contains the spot price and liquidity depth.
/// Note that the fee is included in the spot price already.
#[derive(Debug, Clone, Default)]
pub struct DepthAndPrice {
    /// Spot price (token_out per token_in) for this edge direction.
    pub spot_price: f64,
    /// Liquidity depth normalized to gas token (native token) units.
    pub depth: f64,
}

impl DepthAndPrice {
    /// Creates a new DepthAndPrice with all fields set.
    #[cfg(test)]
    pub fn new(spot_price: f64, depth: f64) -> Self {
        Self { spot_price, depth }
    }

    /// Compute depth and spot price from a live protocol simulation.
    #[cfg(test)]
    pub fn from_protocol_sim<S: ProtocolSim + ?Sized>(
        sim: &S,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<Self, AlgorithmError> {
        Ok(Self {
            spot_price: sim
                .spot_price(token_in, token_out)
                .map_err(|e| {
                    AlgorithmError::Other(format!("missing spot price for DepthAndPrice: {:?}", e))
                })?,
            depth: sim
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .map_err(|e| {
                    AlgorithmError::Other(format!("missing depth for DepthAndPrice: {:?}", e))
                })?
                .0
                .to_f64()
                .ok_or_else(|| {
                    AlgorithmError::Other("depth conversion to f64 failed".to_string())
                })?,
        })
    }
}

impl crate::graph::EdgeWeightFromSimAndDerived for DepthAndPrice {
    fn from_sim_and_derived(
        _sim: &dyn ProtocolSim,
        component_id: &ComponentId,
        token_in: &Token,
        token_out: &Token,
        derived: &crate::derived::DerivedData,
    ) -> Option<Self> {
        let key = (component_id.clone(), token_in.address.clone(), token_out.address.clone());

        // Use pre-computed spot price; skip edge if unavailable.
        let spot_price = match derived
            .spot_prices()
            .and_then(|p| p.get(&key).copied())
        {
            Some(p) => p,
            None => {
                trace!(component_id = %component_id, "spot price not found, skipping edge");
                return None;
            }
        };

        // Look up pre-computed depth; skip edge if unavailable.
        let raw_depth = match derived
            .component_depths()
            .and_then(|d| d.get(&key))
        {
            Some(d) => d.to_f64().unwrap_or(0.0),
            None => {
                trace!(component_id = %component_id, "component depth not found, skipping edge");
                return None;
            }
        };

        // Normalize depth from raw token_in units to gas token units.
        // TokenGasPrices stores Price { numerator, denominator } where
        // numerator/denominator = "token units per gas token unit".
        // To convert to gas token: depth_gas = raw_depth * denominator / numerator.
        let depth = match derived
            .token_prices()
            .and_then(|p| p.get(&token_in.address))
        {
            Some(price) => {
                let num = price_part(&price.numerator, "numerator", component_id, token_in)?;
                let den = price_part(&price.denominator, "denominator", component_id, token_in)?;
                raw_depth * den / num
            }
            None => {
                trace!(
                    component_id = %component_id,
                    token_in = %token_in.address,
                    "token price not found, skipping edge"
                );
                return None;
            }
        };

        Some(Self { spot_price, depth })
    }
}

impl MostLiquidAlgorithm {
    /// Creates a new MostLiquidAlgorithm with default settings.
    pub fn new() -> Self {
        Self {
            query: GraphQueryFilter { min_hops: 1, max_hops: 3, connector_tokens: None },
            timeout: Duration::from_millis(500),
            max_routes: None,
            cache_pair_swaps: true,
        }
    }

    /// Creates a new MostLiquidAlgorithm with custom settings.
    pub fn with_config(config: AlgorithmConfig) -> Result<Self, AlgorithmError> {
        if config.min_hops() == 0 || config.min_hops() > config.max_hops() {
            return Err(AlgorithmError::InvalidConfiguration {
                reason: format!(
                    "invalid hop configuration: min_hops={} max_hops={}",
                    config.min_hops(),
                    config.max_hops()
                ),
            });
        }

        Ok(Self {
            query: GraphQueryFilter {
                min_hops: config.min_hops(),
                max_hops: config.max_hops(),
                connector_tokens: config.connector_tokens().cloned(),
            },
            timeout: config.timeout(),
            max_routes: config.max_routes(),
            cache_pair_swaps: true,
        })
    }

    /// Ranks a token sequence without simulating it, by the best its hops could do.
    ///
    /// Every pool on a hop is read as the market holds it, with nothing carried between hops. That
    /// is exact here because [`MostLiquidAlgorithm::solve_token_path`] never takes the same pool
    /// twice, so no hop is scored against a pool an earlier hop would have moved.
    ///
    /// The same shape as [`MostLiquidAlgorithm::try_score_path`] — spot prices multiplied, thinnest
    /// depth as the bottleneck — but a hop here stands for several pools, so it is scored on its
    /// best. That makes the figure an upper bound on any route through the sequence, which is what
    /// a ranking wants: no sequence is pushed down the queue below one that cannot beat it.
    ///
    /// A hop whose pools all lack derived data scores as unmeasured, which sinks the sequence to
    /// the bottom of the queue without removing it. Depth is what the ranking is made of, so a
    /// sequence missing it cannot be placed -- but it can still be simulated, and simulation is
    /// what decides.
    ///
    /// Returns `None` only when the sequence names a pair the graph has no pool for, which would be
    /// the two indexes disagreeing rather than a routing outcome.
    fn score_token_path(
        graph: &TopologyGraph<DepthAndPrice>,
        token_path: &[NodeIndex],
    ) -> Option<f64> {
        if token_path.len() < 2 {
            return None;
        }

        let mut price = 1.0;
        let mut min_depth = f64::MAX;

        for pair in token_path.windows(2) {
            let pools = graph.pools_between(pair[0], pair[1]);
            if pools.is_empty() {
                return None;
            }

            let mut best_price = f64::MIN;
            let mut best_depth = f64::MIN;
            for pool in pools {
                if let Some(data) = pool.data.as_ref() {
                    best_price = best_price.max(data.spot_price);
                    best_depth = best_depth.max(data.depth);
                }
            }

            if best_price == f64::MIN {
                // Nothing measured on this hop. Neutral on price, thinnest possible on depth, so
                // the sequence sinks to the bottom of the queue rather than out of it.
                min_depth = 0.0;
            } else {
                price *= best_price;
                min_depth = min_depth.min(best_depth);
            }
        }

        Some(price * min_depth)
    }

    /// Builds the route the caller chose, once, from the pools it picked.
    ///
    /// Each hop is simulated again rather than carrying its result through the comparison: the
    /// numbers come out the same, and a swap needs the pool's state as it was before it, which
    /// nothing else has to hold on to. The state overrides are a backstop only -- the solve hands
    /// over a route whose hops name distinct pools -- and they cover protocols whose components
    /// share state behind the scenes.
    fn build_route(
        ctx: &SolveContext<'_>,
        token_path: &[NodeIndex],
        solved: &SolvedRoute,
    ) -> Result<Route, AlgorithmError> {
        let SolveContext { graph, market, amount_in, .. } = *ctx;
        let mut current_amount = amount_in.clone();
        let mut swaps = Vec::with_capacity(solved.hops.len());
        let mut tokens: FxHashMap<Address, Token> = FxHashMap::default();
        let mut state_overrides: FxHashMap<ComponentId, Box<dyn ProtocolSim>> =
            FxHashMap::default();

        for (pair, hop) in token_path.windows(2).zip(&solved.hops) {
            let address_in = &graph[pair[0]];
            let address_out = &graph[pair[1]];
            let token_in = paths::get_token(market, address_in)?;
            let token_out = paths::get_token(market, address_out)?;

            let component_id = graph
                .pools_between(pair[0], pair[1])
                .get(hop.pool_ix)
                .map(|edge| &edge.component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "pool",
                    id: Some(format!("{address_in:?} -> {address_out:?}")),
                })?;
            let component = market
                .get_component(component_id)
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "component",
                    id: Some(component_id.clone()),
                })?;
            let state = state_overrides
                .get(component_id)
                .map(Box::as_ref)
                .or_else(|| market.get_simulation_state(component_id))
                .ok_or_else(|| AlgorithmError::DataNotFound {
                    kind: "simulation state",
                    id: Some(component_id.clone()),
                })?;
            let result = state
                .get_amount_out_guarded(current_amount.clone(), token_in, token_out)
                .map_err(|e| AlgorithmError::Other(format!("simulation error: {e:?}")))?;

            swaps.push(Swap::new(
                component_id.clone(),
                component.protocol_system.clone(),
                token_in.address.clone(),
                token_out.address.clone(),
                current_amount.clone(),
                result.amount.clone(),
                result.gas,
                component.clone(),
                state.clone_box(),
            ));
            tokens
                .entry(token_in.address.clone())
                .or_insert_with(|| token_in.clone());
            tokens
                .entry(token_out.address.clone())
                .or_insert_with(|| token_out.clone());

            state_overrides.insert(component_id.clone(), result.new_state);
            current_amount = result.amount;
        }

        Ok(Route::new(swaps, tokens)?)
    }

    /// Ranks token sequences by [`MostLiquidAlgorithm::score_token_path`], best first.
    ///
    /// A sequence whose pools carry no derived data still scores — as zero depth — and is still
    /// simulated. Only a sequence with a pair no pool trades at all drops out here.
    fn score_paths(
        graph: &TopologyGraph<DepthAndPrice>,
        all_paths: Vec<TokenPath>,
    ) -> Vec<(TokenPath, f64)> {
        let mut scored_paths: Vec<(TokenPath, f64)> = all_paths
            .into_iter()
            .filter_map(|path| {
                let score = Self::score_token_path(graph, &path)?;
                Some((path, score))
            })
            .collect();

        scored_paths.sort_by(|(_, a_score), (_, b_score)| {
            // Flip the comparison to get descending order
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        scored_paths
    }

    async fn snapshot_market_state(
        graph: &TopologyGraph<DepthAndPrice>,
        market: MarketData,
        label: Option<StateLabel>,
        scored_paths: &[(TokenPath, f64)],
    ) -> Result<MarketState, AlgorithmError> {
        let mut pairs: FxHashSet<(NodeIndex, NodeIndex)> = FxHashSet::default();
        for (token_path, _) in scored_paths {
            for pair in token_path.windows(2) {
                pairs.insert((pair[0], pair[1]));
            }
        }
        let mut component_ids: FxHashSet<&ComponentId> = FxHashSet::default();
        for &(from, to) in &pairs {
            for pool in graph.pools_between(from, to) {
                component_ids.insert(&pool.component_id);
            }
        }

        let market = match label.as_ref() {
            Some(l) => market
                .read_labeled(l)
                .await
                .map_err(|e| AlgorithmError::Other(e.to_string()))?,
            None => market.read().await,
        };
        let market_subset = market.extract_subset_with_overlay(&component_ids);
        drop(market);
        Ok(market_subset)
    }

    fn solve_for_best_path(
        &self,
        scored_paths: &[(TokenPath, f64)],
        report: &mut SolveReport,
        ctx: &SolveContext,
    ) -> Result<RouteResult, AlgorithmError> {
        let mut best_route: Option<(&TokenPath, SolvedRoute)> = None;
        let mut cache = PoolSwapsCache::new(self.cache_pair_swaps);
        let timeout_ms = self.timeout.as_millis() as u64;

        for (token_path, _) in scored_paths {
            // Check timeout
            let elapsed_ms = ctx.start.elapsed().as_millis() as u64;
            if elapsed_ms > timeout_ms {
                break;
            }

            let solved = match Self::solve_token_path(ctx, token_path, &mut cache) {
                Ok(solved) => solved,
                Err(e) => {
                    trace!(error = %e, "could not solve path");
                    report.simulation_failures += 1;
                    continue;
                }
            };

            // Check if this is the best result so far
            if best_route
                .as_ref()
                .is_none_or(|(_, previous): &(&TokenPath, SolvedRoute)| {
                    solved.net_amount_out > previous.net_amount_out
                })
            {
                best_route = Some((token_path, solved));
            }

            report.paths_simulated += 1;
        }

        // Only the winner is built into swaps: that is what copies a component and a pool state per
        // hop, and every other candidate would have thrown them away.
        let best = match best_route {
            Some((token_path, solved)) => {
                let route = Self::build_route(ctx, token_path, &solved)?;
                if let Err(e) = route.validate() {
                    trace!(error = %e, "best route failed validation");
                    report.validation_failures += 1;
                    None
                } else {
                    let amount_out = swap_on_route(&route, ctx.token_prices, ctx.gas_price);
                    Some(RouteResult::new(route, amount_out, ctx.gas_price.clone()))
                }
            }
            None => None,
        };

        let solve_time_ms = ctx.start.elapsed().as_millis() as u64;
        report.record(
            best.as_ref(),
            ctx.market,
            ctx.amount_in,
            solve_time_ms,
            ctx.market.component_count(),
        );

        match best {
            Some(best_route) => Ok(best_route),
            None => {
                if solve_time_ms > timeout_ms {
                    Err(AlgorithmError::Timeout { elapsed_ms: solve_time_ms })
                } else {
                    Err(AlgorithmError::InsufficientLiquidity)
                }
            }
        }
    }

    /// Solves a fixed token sequence, choosing the pool to swap through on each hop.
    ///
    /// Walks the hops in order and, at each, simulates every pool serving that pair at the amount
    /// actually arriving and keeps whichever nets most after its own gas. Enumerating the
    /// combinations instead costs the product of the pools per hop; this costs their sum.
    ///
    /// Choosing hop by hop is not a shortcut that gives up accuracy. More into a pool is more out
    /// of it, so the largest amount at each hop carries through to the largest amount at the
    /// end. Gas does not disturb that: the gas already spent is common to every candidate at a
    /// hop and drops out of the comparison, and a hop's own gas priced in the token it pays out
    /// ranks the candidates the same way as comparing whole routes would. What it does improve
    /// on is the ranking heuristic, which never sees the order size — here every choice is made
    /// at the amount really flowing.
    ///
    /// Falls back to comparing gross output when a token has no price, which is exact for gross.
    ///
    /// A pool an earlier hop swapped through is not offered to a later one. Each hop is simulated
    /// against the pool's state as the market holds it, so a pool taken twice would be quoted the
    /// second time as if the first swap had not moved it. Two hops of one sequence never name the
    /// same token pair, but one component can serve two pairs -- a pool holding A, B and C is on
    /// both hops of A -> B -> C -- and a circular sequence crosses the same pair in both
    /// directions. Sequences whose hops all run out of pools this way are dropped, the same as any
    /// other sequence that cannot be settled.
    fn solve_token_path(
        ctx: &SolveContext<'_>,
        token_path: &[NodeIndex],
        cache: &mut PoolSwapsCache,
    ) -> Result<SolvedRoute, MostLiquidError> {
        let mut current_amount = ctx.amount_in.clone();
        let mut hops: SmallVec<[HopResult; INLINE_EDGES]> = SmallVec::new();
        let mut used_components: SmallVec<[&ComponentId; INLINE_EDGES]> = SmallVec::new();
        let mut gas = BigUint::ZERO;
        // What a unit of gas costs in the token the route pays out, which is the last hop's own
        // output token. `None` when that token has no price.
        let mut route_gas_price = None;

        for pair in token_path.windows(2) {
            let (token_in, token_out, token_out_gas_price) = Self::get_pair_data(ctx, pair)?;

            let simulate = |component_id: &ComponentId| {
                let state = ctx
                    .market
                    .get_simulation_state(component_id)?;
                let result = state
                    .get_amount_out_guarded(current_amount.clone(), token_in, token_out)
                    .ok()?;
                let net = match token_out_gas_price {
                    Some(price) => {
                        let cost =
                            &result.gas * ctx.gas_price * &price.numerator / &price.denominator;
                        BigInt::from(result.amount.clone()) - BigInt::from(cost)
                    }
                    None => BigInt::from(result.amount.clone()),
                };
                Some((result, net))
            };

            let pools = ctx
                .graph
                .pools_between(pair[0], pair[1]);

            // A pool this route already crossed cannot be offered again. Where that bites, the
            // pool is picked here and the cache is left alone in both directions: what it holds
            // was chosen over every pool, and the best of a narrowed field is not the answer the
            // next route to ask this pair at this amount should be handed.
            let restricted = pools
                .iter()
                .any(|edge| used_components.contains(&&edge.component_id));
            let hop_result = if restricted {
                best_paying_pool(pools, |id| !used_components.contains(&id), simulate)
            } else {
                cache.swap((pair[0], pair[1]), &current_amount, pools, simulate)
            };
            let Some(hop_result) = hop_result else {
                return Err(MostLiquidError::HopNotTradable { from: pair[0], to: pair[1] })
            };

            let chosen = pools
                .get(hop_result.pool_ix)
                .expect("Pool used not present in the pools available");

            used_components.push(&chosen.component_id);

            gas += &hop_result.gas;
            current_amount = hop_result.amount_out.clone();
            route_gas_price = token_out_gas_price;
            hops.push(hop_result);
        }

        let net_amount_out = match route_gas_price {
            Some(price) => {
                let cost = gas * ctx.gas_price * &price.numerator / &price.denominator;
                BigInt::from(current_amount) - BigInt::from(cost)
            }
            None => BigInt::from(current_amount),
        };

        Ok(SolvedRoute { hops, net_amount_out })
    }

    fn get_pair_data<'a>(
        ctx: &SolveContext<'a>,
        pair: &[NodeIndex],
    ) -> Result<(&'a Token, &'a Token, Option<&'a Price>), MostLiquidError> {
        let token_in = ctx
            .market
            .get_token(&ctx.graph[pair[0]])
            .ok_or(MostLiquidError::TokenMissing(pair[0]))?;
        let token_out = ctx
            .market
            .get_token(&ctx.graph[pair[1]])
            .ok_or(MostLiquidError::TokenMissing(pair[1]))?;
        let token_out_gas_price = ctx
            .token_prices
            .and_then(|prices| prices.get(&token_out.address));
        Ok((token_in, token_out, token_out_gas_price))
    }
}

impl Default for MostLiquidAlgorithm {
    fn default() -> Self {
        Self::new()
    }
}

impl Algorithm for MostLiquidAlgorithm {
    type GraphType = TopologyGraph<DepthAndPrice>;
    type GraphManager = TopologyGraphManager<DepthAndPrice>;

    fn name(&self) -> &str {
        "most_liquid"
    }

    // TODO: Consider adding token pair symbols to the span for easier interpretation
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

        // Exact-out isn't supported yet
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        // Shared rather than copied: a solve only reads these, and the map covers every token in
        // the market.
        let token_prices = match derived.as_ref() {
            Some(derived) => derived
                .read()
                .await
                .token_prices_shared(),
            None => None,
        };

        let amount_in = order.amount().clone();

        // Step 1: Find every route as a sequence of tokens. Pools are chosen per hop during
        // simulation.
        let all_paths =
            paths::find_token_paths(graph, order.token_in(), order.token_out(), &self.query)?;
        let n_paths = all_paths.len();
        let no_path = |reason| AlgorithmError::NoPath {
            from: order.token_in().clone(),
            to: order.token_out().clone(),
            reason,
        };
        if all_paths.is_empty() {
            return Err(no_path(NoPathReason::NoGraphPath));
        }

        // Step 2: Score and sort all paths by estimated output (higher score = better)
        // No lock needed — scoring uses only local graph data.
        let mut scored_paths = Self::score_paths(graph, all_paths);
        if scored_paths.is_empty() {
            return Err(no_path(NoPathReason::NoScorablePaths));
        }

        let mut report = SolveReport {
            paths_candidates: n_paths,
            scoring_failures: n_paths - scored_paths.len(),
            ..SolveReport::default()
        };

        if let Some(max_routes) = self.max_routes {
            scored_paths.truncate(max_routes);
        }
        report.paths_to_simulate = scored_paths.len();

        // Step 3: Fetch all pools in scored_paths.
        let market = Self::snapshot_market_state(graph, market, label, &scored_paths).await?;
        let gas_price = market
            .gas_price()
            .ok_or(AlgorithmError::DataNotFound { kind: "gas price", id: None })?
            .effective_gas_price()
            .clone();

        // Step 4: Solve all paths in score order and return the best one
        let ctx = SolveContext {
            graph,
            market: &market,
            token_prices: token_prices.as_deref(),
            amount_in: &amount_in,
            gas_price: &gas_price,
            start,
        };

        self.solve_for_best_path(&scored_paths, &mut report, &ctx)
    }

    fn computation_requirements(&self) -> ComputationRequirements {
        // MostLiquidAlgorithm uses token prices for two purposes:
        // 1. Converting gas costs from wei to output token terms (net_amount_out)
        // 2. Normalizing component depth to gas token units for path scoring (from_sim_and_derived)
        //
        // Token prices are marked as `allow_stale` since they don't change much
        // block-to-block. Stale prices affect scoring order (not correctness)
        // and gas cost estimation accuracy.
        ComputationRequirements::none()
            .allow_stale("token_prices")
            .expect("Conflicting Computation Requirements")
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// The pool on a pair paying the most net of gas, among those `usable` admits.
///
/// `pool_ix` indexes `pools`, so a caller that skipped some still gets back the index the built
/// route needs.
fn best_paying_pool<D>(
    pools: &[EdgeData<D>],
    mut usable: impl FnMut(&ComponentId) -> bool,
    mut simulate: impl FnMut(&ComponentId) -> Option<(GetAmountOutResult, BigInt)>,
) -> Option<HopResult> {
    let mut best: Option<(usize, GetAmountOutResult, BigInt)> = None;

    for (pool_ix, edge) in pools.iter().enumerate() {
        if !usable(&edge.component_id) {
            continue;
        }
        let Some((result, net)) = simulate(&edge.component_id) else {
            trace!(component_id = edge.component_id, "simulation failed, skipping pool");
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(_, _, best_net)| net > *best_net)
        {
            best = Some((pool_ix, result, net));
        }
    }

    let (pool_ix, result, _) = best?;
    Some(HopResult::new(pool_ix, result))
}

/// What each token pair paid, created for every order and not persisted after it.
///
/// Routes share pairs: every route through WBTC -> WETH asks the same pools the same question. This
/// answers it once. Which pool pays most is remembered for the pair; what it paid is remembered per
/// input amount, so routes arriving with the same amount get the same numbers.
///
/// Remembering nothing is a valid state: [`PoolSwapsCache::new`] takes a flag, and with it off
/// every hop asks every pool, which is the answer the cache is an approximation of.
struct PoolSwapsCache {
    pairs: FxHashMap<(NodeIndex, NodeIndex), PairCacheEntry>,
    enabled: bool,
}

/// The pool that won a pair, and what it paid at each amount seen so far.
struct PairCacheEntry {
    /// Where the pool that last won this pair sits in its pool list.
    pool_ix: usize,
    /// Keyed by the amount that went in. Each outcome names the pool it came from, which is not
    /// always `pool_ix`: a later amount can be won by a different pool, and the outcomes already
    /// recorded still belong to whichever pool produced them.
    outcomes_by_amount: FxHashMap<BigUint, HopResult>,
}

impl PoolSwapsCache {
    fn new(enabled: bool) -> Self {
        Self { pairs: FxHashMap::default(), enabled }
    }

    /// Swaps one pair: which pool to go through, and what it pays.
    ///
    /// Tries in order: the amount already recorded for this pair, then the pool that won it, then
    /// every pool. `simulate` runs one pool and reports what it pays and what that is worth after
    /// its own gas.
    ///
    /// Returns `None` when no pool on the pair can trade the amount.
    fn swap<D>(
        &mut self,
        pair: (NodeIndex, NodeIndex),
        amount_in: &BigUint,
        pools: &[EdgeData<D>],
        mut simulate: impl FnMut(&ComponentId) -> Option<(GetAmountOutResult, BigInt)>,
    ) -> Option<HopResult> {
        if let Some(choice) = self.pairs.get(&pair) {
            if let Some(outcome) = choice.outcomes_by_amount.get(amount_in) {
                return Some(outcome.clone());
            }

            // The pool was simulated but not at this amount. We assume that amounts dont move
            // so much so we assume the same winner wins again at a slightly different amount.
            if let Some((result, _)) = pools
                .get(choice.pool_ix)
                .and_then(|edge| simulate(&edge.component_id))
            {
                let hop_result = HopResult::new(choice.pool_ix, result);
                self.record(pair, amount_in, &hop_result);
                return Some(hop_result);
            }
        }

        let hop_result = best_paying_pool(pools, |_| true, simulate)?;
        self.record(pair, amount_in, &hop_result);
        Some(hop_result)
    }

    /// Remembers what a pool paid for one amount on one pair, unless the cache is off.
    ///
    /// This is the only place anything is written, so an off cache stays empty and every lookup in
    /// [`PoolSwapsCache::swap`] misses.
    fn record(&mut self, pair: (NodeIndex, NodeIndex), amount_in: &BigUint, result: &HopResult) {
        if !self.enabled {
            return;
        }

        let choice = self
            .pairs
            .entry(pair)
            .or_insert_with(|| PairCacheEntry {
                pool_ix: result.pool_ix,
                outcomes_by_amount: FxHashMap::default(),
            });
        choice.pool_ix = result.pool_ix;
        choice
            .outcomes_by_amount
            .insert(amount_in.clone(), result.clone());
    }
}

#[cfg(test)]
mod tests {
    use tycho_simulation::{
        tycho_core::simulation::protocol_sim::Price,
        tycho_ethereum::gas::{BlockGasPrice, GasPrice},
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{
            addr, component,
            fixtures::{addrs, linear_graph},
            order, setup_market_weighted, token, MockProtocolSim, ONE_ETH,
        },
        derived::{
            computation::{FailedItem, FailedItemError},
            types::TokenGasPrices,
            DerivedData,
        },
        graph::GraphManager,
        types::OrderSide,
    };

    fn wrap_market(market: MarketState) -> MarketData {
        MarketData::new(std::sync::Arc::new(tokio::sync::RwLock::new(market)))
    }

    /// Creates a SharedDerivedDataRef with token prices set for testing.
    ///
    /// The price is set to numerator=1, denominator=1, which means:
    /// gas_cost_in_token = gas_cost_wei * 1 / 1 = gas_cost_wei
    fn setup_derived_with_token_prices(token_addresses: &[Address]) -> SharedDerivedDataRef {
        let mut token_prices: TokenGasPrices = FxHashMap::default();
        for addr in token_addresses {
            // Price where 1 wei of gas = 1 unit of token
            token_prices.insert(
                addr.clone(),
                Price { numerator: BigUint::from(1u64), denominator: BigUint::from(1u64) },
            );
        }

        let mut derived_data = DerivedData::new();
        derived_data.set_token_prices(token_prices, vec![], 1, true);
        std::sync::Arc::new(tokio::sync::RwLock::new(derived_data))
    }

    fn make_mock_sim() -> MockProtocolSim {
        MockProtocolSim::new(2.0)
    }

    fn pair_key(comp: &str, b_in: u8, b_out: u8) -> (String, Address, Address) {
        (comp.to_string(), addr(b_in), addr(b_out))
    }

    fn pair_key_str(comp: &str, b_in: u8, b_out: u8) -> String {
        format!("{comp}/{}/{}", addr(b_in), addr(b_out))
    }

    fn make_token_prices(addresses: &[Address]) -> TokenGasPrices {
        let mut prices = TokenGasPrices::default();
        for addr in addresses {
            // 1:1 price (1 token unit = 1 gas token unit)
            prices.insert(
                addr.clone(),
                Price { numerator: BigUint::from(1u64), denominator: BigUint::from(1u64) },
            );
        }
        prices
    }

    #[test]
    fn test_from_sim_and_derived_failed_spot_price_returns_none() {
        let key = pair_key("component1", 0x01, 0x02);
        let key_str = pair_key_str("component1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        // spot price fails, component depth not computed
        derived.set_spot_prices(
            Default::default(),
            vec![FailedItem {
                key: key_str,
                error: FailedItemError::SimulationFailed("sim error".into()),
            }],
            10,
            true,
        );
        derived.set_component_depths(Default::default(), vec![], 10, true);
        derived.set_token_prices(
            make_token_prices(&[tok_in.address.clone(), tok_out.address.clone()]),
            vec![],
            10,
            true,
        );

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        assert!(result.is_none());
    }

    #[test]
    fn test_from_sim_and_derived_failed_component_depth_returns_none() {
        let key = pair_key("component1", 0x01, 0x02);
        let key_str = pair_key_str("component1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        // spot price succeeds
        let mut prices = crate::derived::types::SpotPrices::default();
        prices.insert(key.clone(), 1.5);
        derived.set_spot_prices(prices, vec![], 10, true);
        // component depth fails
        derived.set_component_depths(
            Default::default(),
            vec![FailedItem {
                key: key_str,
                error: FailedItemError::SimulationFailed("depth error".into()),
            }],
            10,
            true,
        );
        derived.set_token_prices(
            make_token_prices(&[tok_in.address.clone(), tok_out.address.clone()]),
            vec![],
            10,
            true,
        );

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        assert!(result.is_none());
    }

    #[test]
    fn test_from_sim_and_derived_both_failed_returns_none() {
        let key = pair_key("component1", 0x01, 0x02);
        let key_str = pair_key_str("component1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        derived.set_spot_prices(
            Default::default(),
            vec![FailedItem {
                key: key_str.clone(),
                error: FailedItemError::SimulationFailed("spot error".into()),
            }],
            10,
            true,
        );
        derived.set_component_depths(
            Default::default(),
            vec![FailedItem {
                key: key_str,
                error: FailedItemError::SimulationFailed("depth error".into()),
            }],
            10,
            true,
        );
        derived.set_token_prices(
            make_token_prices(&[tok_in.address.clone(), tok_out.address.clone()]),
            vec![],
            10,
            true,
        );

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        assert!(result.is_none());
    }

    #[test]
    fn test_from_sim_and_derived_missing_token_price_returns_none() {
        let key = pair_key("component1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();
        // Spot price and component depth both present
        let mut prices = crate::derived::types::SpotPrices::default();
        prices.insert(key.clone(), 1.5);
        derived.set_spot_prices(prices, vec![], 10, true);

        let mut depths = crate::derived::types::ComponentDepths::default();
        depths.insert(key.clone(), BigUint::from(1000u64));
        derived.set_component_depths(depths, vec![], 10, true);

        // No token prices set — normalization should return None

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        assert!(
            result.is_none(),
            "should return None when token price is missing for depth normalization"
        );
    }

    #[test]
    fn test_from_sim_and_derived_normalizes_depth_to_eth() {
        let key = pair_key("component1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();

        // Spot price
        let mut spot = crate::derived::types::SpotPrices::default();
        spot.insert(key.clone(), 2.0);
        derived.set_spot_prices(spot, vec![], 10, true);

        // Raw depth: 2_000_000 token_in units
        let mut depths = crate::derived::types::ComponentDepths::default();
        depths.insert(key.clone(), BigUint::from(2_000_000u64));
        derived.set_component_depths(depths, vec![], 10, true);

        // Token price: 2000 token_in per 1 ETH (numerator=2000, denominator=1)
        // So 2_000_000 raw units / 2000 = 1000 ETH
        let mut token_prices = TokenGasPrices::default();
        token_prices.insert(
            tok_in.address.clone(),
            Price { numerator: BigUint::from(2000u64), denominator: BigUint::from(1u64) },
        );
        derived.set_token_prices(token_prices, vec![], 10, true);

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        let data = result.expect("should return Some when all data present");
        assert!((data.spot_price - 2.0).abs() < f64::EPSILON, "spot price should be 2.0");
        // depth_in_eth = 2_000_000 * 1 / 2000 = 1000.0
        assert!(
            (data.depth - 1000.0).abs() < f64::EPSILON,
            "depth should be 1000.0 ETH, got {}",
            data.depth
        );
    }

    #[test]
    fn test_from_sim_and_derived_normalizes_depth_fractional_price() {
        let key = pair_key("component1", 0x01, 0x02);
        let tok_in = token(0x01, "A");
        let tok_out = token(0x02, "B");

        let mut derived = DerivedData::new();

        let mut spot = crate::derived::types::SpotPrices::default();
        spot.insert(key.clone(), 0.5);
        derived.set_spot_prices(spot, vec![], 10, true);

        // Raw depth: 500 token_in units
        let mut depths = crate::derived::types::ComponentDepths::default();
        depths.insert(key.clone(), BigUint::from(500u64));
        derived.set_component_depths(depths, vec![], 10, true);

        // Token price: numerator=3, denominator=2 -> 1.5 tokens per ETH
        // depth_in_eth = 500 * 2 / 3 = 333.333...
        let mut token_prices = TokenGasPrices::default();
        token_prices.insert(
            tok_in.address.clone(),
            Price { numerator: BigUint::from(3u64), denominator: BigUint::from(2u64) },
        );
        derived.set_token_prices(token_prices, vec![], 10, true);

        let sim = make_mock_sim();
        let result =
            <DepthAndPrice as crate::graph::EdgeWeightFromSimAndDerived>::from_sim_and_derived(
                &sim, &key.0, &tok_in, &tok_out, &derived,
            );

        let data = result.expect("should return Some when all data present");
        let expected_depth = 500.0 * 2.0 / 3.0;
        assert!(
            (data.depth - expected_depth).abs() < 1e-10,
            "depth should be {expected_depth}, got {}",
            data.depth
        );
    }

    // ==================== find_best_route Tests ====================

    #[tokio::test]
    async fn test_find_best_route_single_path() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(*result.route().swaps()[0].amount_in(), BigUint::from(ONE_ETH));
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 2));
    }

    #[tokio::test]
    async fn test_find_best_route_ranks_by_net_amount_out() {
        // Tests that route selection is based on net_amount_out (output - gas cost),
        // not just gross output. Three parallel components with different spot_price/gas combos:
        //
        // Gas price = 100 wei/gas (set by setup_market_weighted)
        //
        // | Component      | spot_price | gas | Output (1000 in) | Gas Cost (gas*100) | Net   |
        // |-----------|------------|-----|------------------|-------------------|-------|
        // | best      | 3          | 10  | 3000             | 1000              | 2000  |
        // | low_out   | 2          | 5   | 2000             | 500               | 1500  |
        // | high_gas  | 4          | 30  | 4000             | 3000              | 1000  |
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![
            ("best", &token_a, &token_b, MockProtocolSim::new(3.0).with_gas(10)),
            ("low_out", &token_a, &token_b, MockProtocolSim::new(2.0).with_gas(5)),
            ("high_gas", &token_a, &token_b, MockProtocolSim::new(4.0).with_gas(30)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 1000, OrderSide::Sell);

        // Set up derived data with token prices so gas can be deducted
        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algorithm
            .find_best_route(manager.graph(), market, None, Some(derived), &order)
            .await
            .unwrap();

        // Should select "best" component for highest net_amount_out (2000)
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "best");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(3000u64));
        assert_eq!(result.net_amount_out(), &BigInt::from(2000)); // 3000 - 1000
    }

    #[tokio::test]
    async fn test_find_best_route_no_path_returns_error() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C"); // Disconnected

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;
        assert!(matches!(result, Err(AlgorithmError::NoPath { .. })));
    }

    #[tokio::test]
    async fn test_find_best_route_multi_hop() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_weighted(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component2", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, ONE_ETH, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        // A->B: ONE_ETH*2, B->C: (ONE_ETH*2)*3
        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 2));
        assert_eq!(result.route().swaps()[0].component_id(), "component1".to_string());
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(ONE_ETH * 2 * 3));
        assert_eq!(result.route().swaps()[1].component_id(), "component2".to_string());
    }

    /// Builds a market and graph from components that may hold more than two tokens.
    ///
    /// [`setup_market_weighted`] takes one pair per component, which cannot express the pool that
    /// serves two hops of the same sequence. Sets no edge weights: an unranked sequence is still
    /// simulated, and these cases are about which pool a hop picks.
    fn setup_market_multi_token(
        components: Vec<(&str, Vec<Token>, MockProtocolSim)>,
        tokens: &[Token],
    ) -> (MarketData, TopologyGraphManager<DepthAndPrice>) {
        let mut market = MarketState::new();
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });
        market.upsert_tokens(tokens.to_vec());
        let protocol_components: Vec<_> = components
            .iter()
            .map(|(id, tokens, _)| component(id, tokens))
            .collect();
        market.upsert_components(protocol_components);
        let states: Vec<_> = components
            .into_iter()
            .map(|(id, _, sim)| (id.to_string(), Box::new(sim) as Box<dyn ProtocolSim>))
            .collect();
        market.update_states(states);

        let mut manager = TopologyGraphManager::default();
        manager.initialize_graph(&market.component_topology());
        (wrap_market(market), manager)
    }

    /// A pool serving two hops of one sequence is taken by the first of them only.
    ///
    /// `multi` holds all three tokens, so it trades both A -> B and B -> C, and it pays more than
    /// `bc` on the second hop. Taking it twice would price the second swap against a pool the
    /// first swap had already moved, so the second hop falls to `bc`.
    #[tokio::test]
    async fn test_find_best_route_never_takes_one_pool_twice() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_multi_token(
            vec![
                (
                    "multi",
                    vec![token_a.clone(), token_b.clone(), token_c.clone()],
                    MockProtocolSim::new(3.0),
                ),
                ("bc", vec![token_b.clone(), token_c.clone()], MockProtocolSim::new(2.0)),
            ],
            &[token_a.clone(), token_b.clone(), token_c.clone()],
        );

        // Two hops exactly, so the direct A -> C pool `multi` also offers is not the answer.
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, 1000, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        assert_eq!(result.route().swaps().len(), 2);
        assert_eq!(result.route().swaps()[0].component_id(), "multi");
        assert_eq!(result.route().swaps()[1].component_id(), "bc");
        // A -> B through multi: 1000 * 3. B -> C through bc: 3000 * 2.
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(3000u64));
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(6000u64));
    }

    /// A sequence whose second hop has no pool left is dropped rather than solved.
    ///
    /// `multi` is the only pool on either hop of A -> B -> C, and the first hop takes it.
    #[tokio::test]
    async fn test_find_best_route_drops_sequence_with_no_pool_left() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_multi_token(
            vec![(
                "multi",
                vec![token_a.clone(), token_b.clone(), token_c.clone()],
                MockProtocolSim::new(3.0),
            )],
            &[token_a.clone(), token_b.clone(), token_c.clone()],
        );

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, 1000, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;

        assert!(matches!(result, Err(AlgorithmError::InsufficientLiquidity)));
    }

    /// A pool with no derived data is still a pool that trades.
    ///
    /// Depth is measured by binary search over simulations and gives up for a range of reasons --
    /// missing token metadata, a spot price too small to work with, a protocol that answers
    /// `get_limits` poorly. None of those say the pool would execute badly, only that it could not
    /// be measured, so it is ranked last and simulated rather than dropped.
    #[tokio::test]
    async fn test_find_best_route_uses_pools_without_edge_weights() {
        // Component1 has edge weights (scoreable), Component2 doesn't
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Set up market with both components using new API
        let mut market = MarketState::new();
        let component1_state = MockProtocolSim::new(2.0);
        let component2_state = MockProtocolSim::new(3.0); // Higher multiplier but no edge weight

        let component1_comp = component("component1", &[token_a.clone(), token_b.clone()]);
        let component2_comp = component("component2", &[token_a.clone(), token_b.clone()]);

        // Set gas price (required for simulation)
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price: BigUint::from(1u64) },
        });

        // Insert components
        market.upsert_components(vec![component1_comp, component2_comp]);

        // Insert states
        market.update_states(vec![
            ("component1".to_string(), Box::new(component1_state.clone()) as Box<dyn ProtocolSim>),
            ("component2".to_string(), Box::new(component2_state) as Box<dyn ProtocolSim>),
        ]);

        // Insert tokens
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);

        // Initialize graph with both components
        let mut manager = TopologyGraphManager::default();
        manager.initialize_graph(&market.component_topology());

        // Only set edge weights for component1, NOT component2
        let weight =
            DepthAndPrice::from_protocol_sim(&component1_state, &token_a, &token_b).unwrap();
        manager
            .set_pool_weight(
                &"component1".to_string(),
                &token_a.address,
                &token_b.address,
                weight,
                false,
            )
            .unwrap();

        // Use max_hops=1 to focus only on direct 1-hop paths
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let market = wrap_market(market);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        // component2 has no weight, so it never gets ranked -- but it pays 3x against component1's
        // 2x, and simulating both is what settles the hop.
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "component2");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 3));
    }

    /// A market whose derived data has not been computed at all still routes.
    ///
    /// Ranking only decides what order routes are simulated in, so with nothing to rank by every
    /// route is still simulated.
    #[tokio::test]
    async fn test_find_best_route_without_any_derived_data() {
        // All paths exist but none have edge weights
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let mut market = MarketState::new();
        let component_state = MockProtocolSim::new(2.0);
        let comp = component("component1", &[token_a.clone(), token_b.clone()]);

        // Set gas price (required for simulation)
        market.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Eip1559 {
                base_fee_per_gas: BigUint::from(1u64),
                max_priority_fee_per_gas: BigUint::from(0u64),
            },
        });

        market.upsert_components(vec![comp]);
        market.update_states(vec![(
            "component1".to_string(),
            Box::new(component_state) as Box<dyn ProtocolSim>,
        )]);
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);

        // Initialize graph but DO NOT set any edge weights
        let mut manager = TopologyGraphManager::default();
        manager.initialize_graph(&market.component_topology());

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let market = wrap_market(market);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .expect("an unranked route is still a route");

        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "component1");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(ONE_ETH * 2));
    }

    #[tokio::test]
    async fn test_find_best_route_gas_exceeds_output_returns_negative_net() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);
        let mut market_write = market.try_write().unwrap();

        // Set a non-zero gas price so gas cost exceeds tiny output
        // gas_cost = 50_000 * (1_000_000 + 1_000_000) = 100_000_000_000 >> 2 wei output
        market_write.update_gas_price(BlockGasPrice {
            block_number: 1,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Eip1559 {
                base_fee_per_gas: BigUint::from(1_000_000u64),
                max_priority_fee_per_gas: BigUint::from(1_000_000u64),
            },
        });
        drop(market_write); // Release write lock

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, 1, OrderSide::Sell); // 1 wei input -> 2 wei output

        // Set up derived data with token prices so gas can be deducted
        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        // Route should still be returned, but with negative net_amount_out
        let result = algorithm
            .find_best_route(manager.graph(), market, None, Some(derived), &order)
            .await
            .expect("should return route even with negative net_amount_out");

        // Verify the route has swaps
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(2u64)); // 1 * 2 = 2 wei

        // Verify it's: 2 - 200_000_000_000 = -199_999_999_998
        let expected_net = BigInt::from(2) - BigInt::from(100_000_000_000u64);
        assert_eq!(result.net_amount_out(), &expected_net);
    }

    #[tokio::test]
    async fn test_find_best_route_insufficient_liquidity() {
        // Component has limited liquidity (1000 wei) but we try to swap ONE_ETH
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0).with_liquidity(1000),
        )]);

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell); // More than 1000 wei liquidity

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;
        assert!(matches!(result, Err(AlgorithmError::InsufficientLiquidity)));
    }

    #[tokio::test]
    async fn test_find_best_route_missing_gas_price_returns_error() {
        // Test that missing gas price returns DataNotFound error, not InsufficientLiquidity
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let mut market = MarketState::new();
        let component_state = MockProtocolSim::new(2.0);
        let comp = component("component1", &[token_a.clone(), token_b.clone()]);

        // DO NOT set gas price - this is what we're testing
        market.upsert_components(vec![comp]);
        market.update_states(vec![(
            "component1".to_string(),
            Box::new(component_state.clone()) as Box<dyn ProtocolSim>,
        )]);
        market.upsert_tokens(vec![token_a.clone(), token_b.clone()]);

        // Initialize graph and set edge weights
        let mut manager = TopologyGraphManager::default();
        manager.initialize_graph(&market.component_topology());
        let weight =
            DepthAndPrice::from_protocol_sim(&component_state, &token_a, &token_b).unwrap();
        manager
            .set_pool_weight(
                &"component1".to_string(),
                &token_a.address,
                &token_b.address,
                weight,
                false,
            )
            .unwrap();

        let algorithm = MostLiquidAlgorithm::new();
        let order = order(&token_a, &token_b, ONE_ETH, OrderSide::Sell);
        let market = wrap_market(market);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;

        // Should get DataNotFound for gas price, not InsufficientLiquidity
        assert!(matches!(result, Err(AlgorithmError::DataNotFound { kind: "gas price", .. })));
    }

    /// A circle crosses the pair in both directions, so it needs a pool for each direction.
    #[tokio::test]
    async fn test_find_best_route_circular_arbitrage() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // MockProtocolSim::get_amount_out multiplies by spot_price when token_in < token_out and
        // divides by it otherwise.
        let (market, manager) = setup_market_weighted(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component2", &token_a, &token_b, MockProtocolSim::new(3.0)),
        ]);

        // Use min_hops=2 to require at least 2 hops (circular)
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();

        // Order: swap A for A (circular)
        let order = order(&token_a, &token_a, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        // Should have 2 swaps forming a circle
        assert_eq!(result.route().swaps().len(), 2, "Should have 2 swaps for circular route");

        // First swap: A -> B through component2, which pays most (100 * 3 = 300)
        assert_eq!(*result.route().swaps()[0].token_in(), token_a.address);
        assert_eq!(*result.route().swaps()[0].token_out(), token_b.address);
        assert_eq!(result.route().swaps()[0].component_id(), "component2");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(300u64));

        // Second swap: B -> A. component2 is spent, so the hop falls to component1 (300 / 2 = 150)
        assert_eq!(*result.route().swaps()[1].token_in(), token_b.address);
        assert_eq!(*result.route().swaps()[1].token_out(), token_a.address);
        assert_eq!(result.route().swaps()[1].component_id(), "component1");
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(150u64));

        // Verify the route starts and ends with the same token
        assert_eq!(result.route().swaps()[0].token_in(), result.route().swaps()[1].token_out());
    }

    /// One pool is not a circle: the return hop would have to cross the pool the outbound hop
    /// already moved, which is the reuse the solve refuses.
    #[tokio::test]
    async fn test_find_best_route_circular_needs_two_pools() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_a, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;

        assert!(matches!(result, Err(AlgorithmError::InsufficientLiquidity)));
    }

    #[tokio::test]
    async fn test_find_best_route_respects_min_hops() {
        // Setup: A->B (1-hop) and A->C->B (2-hop)
        // With min_hops=2, should only return the 2-hop path
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_weighted(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(10.0)), /* Direct: 1-hop,
                                                                               * high
                                                                               * output */
            ("component_ac", &token_a, &token_c, MockProtocolSim::new(2.0)), // 2-hop path
            ("component_cb", &token_c, &token_b, MockProtocolSim::new(3.0)), // 2-hop path
        ]);

        // min_hops=2 should skip the 1-hop direct path
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 3, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 100, OrderSide::Sell);

        // Set up derived data with token prices so gas can be deducted
        // This ensures shorter paths are preferred due to lower gas cost
        let derived = setup_derived_with_token_prices(std::slice::from_ref(&token_b.address));

        let result = algorithm
            .find_best_route(manager.graph(), market, None, Some(derived), &order)
            .await
            .unwrap();

        // Should use 2-hop path (A->C->B), not the direct 1-hop path
        assert_eq!(result.route().swaps().len(), 2, "Should use 2-hop path due to min_hops=2");
        assert_eq!(result.route().swaps()[0].component_id(), "component_ac");
        assert_eq!(result.route().swaps()[1].component_id(), "component_cb");
    }

    #[tokio::test]
    async fn test_find_best_route_respects_max_hops() {
        // Setup: Only path is A->B->C (2 hops)
        // With max_hops=1, no sequence reaches C, so nothing can be scored
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        let (market, manager) = setup_market_weighted(vec![
            ("component_ab", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component_bc", &token_b, &token_c, MockProtocolSim::new(3.0)),
        ]);

        // max_hops=1 cannot reach C from A (needs 2 hops)
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;
        assert!(
            matches!(result, Err(AlgorithmError::NoPath { reason: NoPathReason::NoGraphPath, .. })),
            "Should fail when max_hops is insufficient"
        );
    }

    #[tokio::test]
    async fn test_find_best_route_timeout_returns_best_so_far() {
        // Setup: Many parallel paths to process
        // With very short timeout, should return the best route found before timeout
        // or Timeout error if no route was completed
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // Create many parallel components to ensure multiple paths need processing
        let (market, manager) = setup_market_weighted(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(1.0)),
            ("component2", &token_a, &token_b, MockProtocolSim::new(2.0)),
            ("component3", &token_a, &token_b, MockProtocolSim::new(3.0)),
            ("component4", &token_a, &token_b, MockProtocolSim::new(4.0)),
            ("component5", &token_a, &token_b, MockProtocolSim::new(5.0)),
        ]);

        // timeout=0ms should timeout after processing some paths
        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(0), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 100, OrderSide::Sell);

        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await;

        // With 0ms timeout, we either get:
        // - A route (if at least one path completed before timeout check)
        // - Timeout error (if no path completed)
        // Both are valid outcomes - the key is we don't hang
        match result {
            Ok(r) => {
                // If we got a route, verify it's valid
                assert_eq!(r.route().swaps().len(), 1);
            }
            Err(AlgorithmError::Timeout { .. }) => {
                // Timeout is also acceptable
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    // ==================== Algorithm Trait Getter Tests ====================

    #[rstest::rstest]
    #[case::default_config(1, 3, 50)]
    #[case::single_hop_only(1, 1, 100)]
    #[case::multi_hop_min(2, 5, 200)]
    #[case::zero_timeout(1, 3, 0)]
    #[case::large_values(10, 100, 10000)]
    fn test_algorithm_config_getters(
        #[case] min_hops: usize,
        #[case] max_hops: usize,
        #[case] timeout_ms: u64,
    ) {
        use crate::algorithm::Algorithm;

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(min_hops, max_hops, Duration::from_millis(timeout_ms), None)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(algorithm.query.max_hops, max_hops);
        assert_eq!(algorithm.timeout, Duration::from_millis(timeout_ms));
        assert_eq!(algorithm.name(), "most_liquid");
    }

    #[test]
    fn test_algorithm_default_config() {
        use crate::algorithm::Algorithm;

        let algorithm = MostLiquidAlgorithm::new();

        assert_eq!(algorithm.query.max_hops, 3);
        assert_eq!(algorithm.timeout, Duration::from_millis(500));
        assert_eq!(algorithm.name(), "most_liquid");
    }

    // ==================== Hop solving Tests ====================

    /// Each hop is solved on its own, at the amount that actually reaches it.
    ///
    /// The pool paying most is deliberately not the first on either hop, and it is not the one the
    /// score ranking favours either — `spot_price * min_depth` prefers the deep, cheap pools here.
    /// Only simulating at the order's size finds them, and picking per hop is what makes that
    /// affordable: four simulations rather than the four two-hop routes they combine into.
    #[tokio::test]
    async fn test_each_leg_takes_its_best_paying_pool() {
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let token_c = token(0x03, "C");

        // Ids sort ascending, so the weak pool of each pair is reached first.
        let (market, manager) = setup_market_weighted(vec![
            ("ab1", &token_a, &token_b, MockProtocolSim::new(1.0).with_liquidity(9_000_000)),
            ("ab2", &token_a, &token_b, MockProtocolSim::new(3.0).with_liquidity(1_000_000)),
            ("bc1", &token_b, &token_c, MockProtocolSim::new(1.0).with_liquidity(9_000_000)),
            ("bc2", &token_b, &token_c, MockProtocolSim::new(5.0).with_liquidity(1_000_000)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(2, 2, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_c, 1000, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        let chosen: Vec<&str> = result
            .route()
            .swaps()
            .iter()
            .map(|swap| swap.component_id())
            .collect();
        assert_eq!(chosen, vec!["ab2", "bc2"], "each hop must take its best-paying pool");

        // 1000 -> 3000 over the first hop, and the second hop has to be settled on that 3000
        // rather than on the order amount.
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(3000u64));
        assert_eq!(*result.route().swaps()[1].amount_in(), BigUint::from(3000u64));
        assert_eq!(*result.route().swaps()[1].amount_out(), BigUint::from(15000u64));
    }

    /// `max_routes` bounds token sequences, not the pools on a pair.
    ///
    /// Every pool on a pair is compared at the amount actually flowing, whatever the cap, so the
    /// pool paying four times the input is found even at `max_routes = 2` -- where the score
    /// ranking, which never sees the order size, puts it last.
    #[tokio::test]
    async fn test_max_routes_caps_token_sequences_not_pools_on_a_pair() {
        // 4 parallel components. Score = spot_price * min_depth.
        // In tests, depth comes from get_limits().0 (sell_limit), which is
        // liquidity / (spot_price * (1 - fee)). With fee=0: depth = liquidity / spot_price.
        // We vary liquidity to create a clear score ranking:
        //   component4 (score = 1.0 * 4M/1.0 = 4M)
        //   component3 (score = 2.0 * 3M/2.0 = 3M)
        //   component2 (score = 3.0 * 2M/3.0 = 2M)
        //   component1 (score = 4.0 * 1M/4.0 = 1M)
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(4.0).with_liquidity(1_000_000)),
            ("component2", &token_a, &token_b, MockProtocolSim::new(3.0).with_liquidity(2_000_000)),
            ("component3", &token_a, &token_b, MockProtocolSim::new(2.0).with_liquidity(3_000_000)),
            ("component4", &token_a, &token_b, MockProtocolSim::new(1.0).with_liquidity(4_000_000)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), Some(2)).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 1000, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        // A -> B is one sequence however many pools serve it, so the cap has nothing to remove and
        // every pool is compared at the order's own size. component1 pays the most.
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "component1");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(4000u64));
    }

    #[tokio::test]
    async fn test_find_best_route_no_cap_when_max_routes_is_none() {
        // Same setup but no cap — component1 (best output) should win.
        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        let (market, manager) = setup_market_weighted(vec![
            ("component1", &token_a, &token_b, MockProtocolSim::new(4.0).with_liquidity(1_000_000)),
            ("component2", &token_a, &token_b, MockProtocolSim::new(3.0).with_liquidity(2_000_000)),
            ("component3", &token_a, &token_b, MockProtocolSim::new(2.0).with_liquidity(3_000_000)),
            ("component4", &token_a, &token_b, MockProtocolSim::new(1.0).with_liquidity(4_000_000)),
        ]);

        let algorithm = MostLiquidAlgorithm::with_config(
            AlgorithmConfig::new(1, 1, Duration::from_millis(100), None).unwrap(),
        )
        .unwrap();
        let order = order(&token_a, &token_b, 1000, OrderSide::Sell);
        let result = algorithm
            .find_best_route(manager.graph(), market, None, None, &order)
            .await
            .unwrap();

        // All 4 paths simulated, component1 wins with best output (4x)
        assert_eq!(result.route().swaps().len(), 1);
        assert_eq!(result.route().swaps()[0].component_id(), "component1");
        assert_eq!(*result.route().swaps()[0].amount_out(), BigUint::from(4000u64));
    }

    // ==================== Configuration Validation Tests ====================

    #[test]
    fn test_algorithm_config_rejects_zero_max_routes() {
        let result = AlgorithmConfig::new(1, 3, Duration::from_millis(100), Some(0));
        assert!(matches!(
            result,
            Err(AlgorithmError::InvalidConfiguration { reason }) if reason.contains("max_routes must be at least 1")
        ));
    }

    #[test]
    fn test_algorithm_config_rejects_zero_min_hops() {
        let result = AlgorithmConfig::new(0, 3, Duration::from_millis(100), None);
        assert!(matches!(
            result,
            Err(AlgorithmError::InvalidConfiguration { reason }) if reason.contains("min_hops must be at least 1")
        ));
    }

    #[test]
    fn test_algorithm_config_rejects_min_greater_than_max() {
        let result = AlgorithmConfig::new(5, 3, Duration::from_millis(100), None);
        assert!(matches!(
            result,
            Err(AlgorithmError::InvalidConfiguration { reason }) if reason.contains("cannot exceed")
        ));
    }

    // ==================== score_token_path Tests ====================

    /// A hop with no measured pool cannot be placed, so the sequence sinks to the bottom of the
    /// queue rather than out of it: it still scores, and the score is the lowest there is.
    #[test]
    fn test_score_token_path_sinks_a_sequence_with_an_unmeasured_hop() {
        let (a, b, c, _) = addrs();
        let mut manager = linear_graph();
        // A->B measured, B->C left without derived data.
        manager
            .set_pool_weight(&"ab".to_string(), &a, &b, DepthAndPrice::new(2.0, 1000.0), false)
            .unwrap();
        let graph = manager.graph();
        let node = |address: &Address| graph.get_token_ix(address).unwrap();

        let unmeasured =
            MostLiquidAlgorithm::score_token_path(graph, &[node(&a), node(&b), node(&c)]);

        assert_eq!(unmeasured, Some(0.0), "an unmeasured hop scores zero, not None");
        assert!(
            MostLiquidAlgorithm::score_token_path(graph, &[node(&a), node(&b)])
                .is_some_and(|measured| measured > 0.0),
            "a fully measured sequence still outranks it"
        );
    }

    /// A sequence naming a pair the graph has no pool for is the two indexes disagreeing, not a
    /// routing outcome, so it is dropped rather than ranked.
    #[test]
    fn test_score_token_path_drops_a_sequence_with_an_unconnected_pair() {
        let (a, _, c, _) = addrs();
        let manager = linear_graph();
        let graph = manager.graph();
        let node = |address: &Address| graph.get_token_ix(address).unwrap();

        assert_eq!(MostLiquidAlgorithm::score_token_path(graph, &[node(&a), node(&c)]), None);
        assert_eq!(MostLiquidAlgorithm::score_token_path(graph, &[node(&a)]), None);
    }

    // ==================== PoolSwapsCache Tests ====================

    /// `count` pools on one pair, named `pool0`, `pool1`, ... for [`simulator`] to answer as.
    fn pools(count: usize) -> Vec<EdgeData<()>> {
        (0..count)
            .map(|i| EdgeData::new(format!("pool{i}")))
            .collect()
    }

    /// Answers as pool `i` would: out is `amount_in * multipliers[i]`, gas is flat.
    fn simulator(
        multipliers: [u64; 2],
        amount_in: u64,
    ) -> impl FnMut(&ComponentId) -> Option<(GetAmountOutResult, BigInt)> {
        move |component_id: &ComponentId| {
            let index: usize = component_id
                .trim_start_matches("pool")
                .parse()
                .ok()?;
            let amount = BigUint::from(amount_in * multipliers[index]);
            let result = GetAmountOutResult::new(
                amount.clone(),
                BigUint::from(10u64),
                Box::new(MockProtocolSim::new(1.0)),
            );
            Some((result, BigInt::from(amount)))
        }
    }

    /// The one token pair every cache case works on.
    fn pair() -> (NodeIndex, NodeIndex) {
        (NodeIndex::new(0), NodeIndex::new(1))
    }

    #[test]
    fn test_cache_takes_the_pool_that_pays_most() {
        let mut cache = PoolSwapsCache::new(true);
        let multipliers = [2u64, 5u64];
        let pools = pools(multipliers.len());

        let outcome = cache
            .swap(pair(), &BigUint::from(100u64), &pools, simulator(multipliers, 100))
            .unwrap();

        assert_eq!(outcome.pool_ix, 1, "pool1 pays 500 against pool0's 200");
        assert_eq!(outcome.amount_out, BigUint::from(500u64));
    }

    /// The second ask at the same amount must not simulate anything.
    #[test]
    fn test_cache_answers_a_repeated_amount_without_simulating() {
        let mut cache = PoolSwapsCache::new(true);
        let multipliers = [2u64, 5u64];
        let pools = pools(multipliers.len());
        let amount = BigUint::from(100u64);
        cache
            .swap(pair(), &amount, &pools, simulator(multipliers, 100))
            .unwrap();

        let outcome = cache
            .swap(pair(), &amount, &pools, |_| panic!("must not simulate on a hit"))
            .unwrap();

        assert_eq!(outcome.pool_ix, 1);
        assert_eq!(outcome.amount_out, BigUint::from(500u64));
    }

    /// A remembered outcome carries the pool that produced it. A later amount won by a different
    /// pool must not rewrite what an earlier one was told.
    #[test]
    fn test_cache_keeps_each_amount_with_the_pool_that_paid_it() {
        let mut cache = PoolSwapsCache::new(true);
        let pools = pools(2);

        // At 100, pool1 wins. At 50 the simulator is rigged so only pool0 answers, which makes it
        // the pair's remembered winner.
        cache
            .swap(pair(), &BigUint::from(100u64), &pools, simulator([2, 5], 100))
            .unwrap();
        cache
            .swap(pair(), &BigUint::from(50u64), &pools, |component_id: &ComponentId| {
                (component_id == "pool0").then(|| {
                    let amount = BigUint::from(50u64);
                    (
                        GetAmountOutResult::new(
                            amount.clone(),
                            BigUint::from(10u64),
                            Box::new(MockProtocolSim::new(1.0)),
                        ),
                        BigInt::from(amount),
                    )
                })
            })
            .unwrap();

        let replay = cache
            .swap(pair(), &BigUint::from(100u64), &pools, |_| panic!("must not simulate on a hit"))
            .unwrap();

        assert_eq!(replay.pool_ix, 1, "the 100 outcome still belongs to pool1");
        assert_eq!(replay.amount_out, BigUint::from(500u64));
    }

    /// Off, the cache stays empty, so every ask simulates again.
    #[test]
    fn test_cache_disabled_remembers_nothing() {
        let mut cache = PoolSwapsCache::new(false);
        let multipliers = [2u64, 5u64];
        let pools = pools(multipliers.len());
        let amount = BigUint::from(100u64);

        let first = cache
            .swap(pair(), &amount, &pools, simulator(multipliers, 100))
            .unwrap();
        let mut asked = 0usize;
        let second = cache
            .swap(pair(), &amount, &pools, |component_id| {
                asked += 1;
                simulator(multipliers, 100)(component_id)
            })
            .unwrap();

        assert_eq!(asked, 2, "both pools are asked again, so nothing was remembered");
        assert_eq!(first.amount_out, second.amount_out);
    }

    /// No pool on the pair can trade the amount.
    #[test]
    fn test_cache_returns_none_when_no_pool_trades() {
        let mut cache = PoolSwapsCache::new(true);
        let pools = pools(2);

        let outcome = cache.swap(pair(), &BigUint::from(100u64), &pools, |_| None);

        assert!(outcome.is_none());
    }
}
