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

use std::{
    cmp::{Ordering, Reverse},
    time::{Duration, Instant},
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
    sim_meter::{MeteredProtocolSim, SolveMeter},
    split_primitives::{build_split_route, HopDescriptor, PathAllocation, SimulatedHop},
    Algorithm, AlgorithmConfig, NoPathReason,
};
use crate::{
    algorithm::paths::read_market,
    derived::{computation::ComputationRequirements, types::TokenGasPrices, SharedDerivedDataRef},
    feed::market_data::{MarketData, MarketDataView, MarketState, StateLabel},
    graph::{EdgeData, GraphQueryFilter, Path, TopologyGraph, TopologyGraphManager},
    types::{ComponentId, Order, Route, RouteResult},
    AlgorithmError,
};

/// Maximum candidate paths simulated per order after heuristic ranking.
const DEFAULT_MAX_CANDIDATES: usize = 5000;
/// Cap on candidates from the bounded amount-aware discovery added to the candidate set
/// (matches the bounded discovery's own cap; see the discovery section below).
const MAX_DISCOVERY_CANDIDATES: usize = 128;
/// Maximum number of parallel paths in a split.
const DEFAULT_MAX_PATHS: usize = 4;
/// Chunk grid for the coarse set-selection pass.
const COARSE_CHUNKS: usize = 20;
/// Chunk grid for the fine allocation pass over the fixed active set.
const FINE_CHUNKS: usize = 256;
/// Number of top full-amount paths always considered for shared-component fill-and-spill.
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
/// Parallel components kept for a discovery edge directly into the target token.
const CANDIDATE_DIRECT_EDGES_PER_TOKEN: usize = 4;
/// Parallel components kept for a discovery edge into an anchor or configured connector token.
const CANDIDATE_CONNECTOR_EDGES_PER_TOKEN: usize = 2;
/// Number of highest-connectivity tokens taken as bounded-discovery anchors, derived per solve
/// from the graph (see `derive_anchor_tokens`).
const DERIVED_ANCHOR_COUNT: usize = 16;
/// Exchange-refinement step floor: the pass stops once `delta` falls below one fine chunk divided
/// by this factor, i.e. `amount_in / (fine_chunks * EXCHANGE_DELTA_FLOOR)`.
const EXCHANGE_DELTA_FLOOR: usize = 64;
/// Safety bound on trial simulations across the whole exchange-refinement pass.
const EXCHANGE_MAX_SIMS: usize = 400;
/// How far apart the two amounts either side of a requested one may sit, as a percentage of the
/// amount requested, before ranking stops reading across them and asks the pool instead.
const INTERPOLATION_GAP_PERCENT: u32 = 10;

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

/// Splits an order across component-disjoint (and, via fill-and-spill, component-sharing) paths to
/// reduce price impact, returning the best net of the single path and several split allocations.
pub struct WaterFillAlgorithm {
    /// The hop bounds and connector tokens every route search runs under.
    query: GraphQueryFilter,
    timeout: Duration,
    max_candidates: usize,
    max_paths: usize,
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

/// One simulated traversal of a path, with the resulting per-component states so they can be
/// committed.
struct StepResult {
    amount_out: BigUint,
    gas: BigUint,
    new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)>,
}

/// The stage of a solve a swap was asked for.
///
/// Recorded against every swap so the report can say how much of a solve sits where answers can be
/// reused, and how much is in the passes that read state they have committed to and can never come
/// through the cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SolvePass {
    /// Bounded discovery expanding its frontier.
    Discovery,
    /// Ranking every candidate path at the full order amount.
    Ranking,
    /// Choosing which candidates fill-and-spill will split across, by probing each with a first
    /// chunk.
    SetSelection,
    /// Exchange refinement re-pricing a path on its own.
    Exchange,
    /// The chunked water-fills, which read what they have committed and never reach the cache.
    Chunking,
    /// Building the legs of a route that will be returned.
    Assembly,
}

impl SolvePass {
    /// Whether this pass can take an amount read across two nearby ones.
    ///
    /// The passes that settle which paths get split can: reading across errs low, so a path may
    /// lose a place it deserved but never take one it did not, and every path they put forward is
    /// simulated for real before anything is allocated to it.
    ///
    /// Discovery is held out even though it also only orders things. Its amounts feed the next hop
    /// rather than staying with one path, so reading low compounds along the frontier and moves
    /// which edges survive pruning — a different candidate set, not a differently ordered one.
    ///
    /// The rest cannot. Exchange refinement shifts flow on a strictly-improving comparison, where
    /// a read-across amount could invent a gain that is not there; the chunked fills and the route
    /// builders decide and report amounts that are handed back to the caller.
    pub(crate) fn may_interpolate(self) -> bool {
        match self {
            SolvePass::Ranking | SolvePass::SetSelection => true,
            SolvePass::Discovery |
            SolvePass::Exchange |
            SolvePass::Chunking |
            SolvePass::Assembly => false,
        }
    }
}

/// A pool and the direction taken through it. Both token addresses are part of it — a pool trading
/// three tokens answers `USDC -> DAI` and `USDT -> DAI` differently for the same amount.
#[derive(PartialEq, Eq, Hash)]
struct PoolDirection<'a> {
    component_id: &'a ComponentId,
    address_in: &'a Address,
    address_out: &'a Address,
}

/// What a swap paid: what came out, and the gas it cost. Used both for one hop and, summed, for
/// a whole path.
#[derive(Clone)]
struct HopOutcome {
    amount_out: BigUint,
    gas: BigUint,
}

/// What one pool paid at each amount it was asked, ascending by amount. A missing outcome is an
/// amount it refused.
///
/// Short by nature — a handful of amounts per direction over one solve — so a sorted `Vec` searched
/// by bisection beats a map, and inserting in place keeps the neighbours of any amount adjacent.
#[derive(Default)]
struct AmountsSwapped {
    outcome_by_amount: Vec<(BigUint, Option<HopOutcome>)>,
    /// The amount from which this pool has been taken to refuse everything. See
    /// [`AmountsSwapped::remember`] for what has to hold before an amount is recorded here.
    refused_from: Option<BigUint>,
}

impl AmountsSwapped {
    /// Whether `amount_in` is at or above the point this pool started refusing.
    fn refuses(&self, amount_in: &BigUint) -> bool {
        self.refused_from
            .as_ref()
            .is_some_and(|refused_from| amount_in >= refused_from)
    }

    /// Records what the pool paid for `amount_in`.
    ///
    /// A pool usually refuses because the swap is bigger than it can serve, so a refusal normally
    /// means every larger amount is refused too. When that looks true here, `refused_from` is set
    /// to this amount (or lowered to it) and larger amounts are refused without asking the pool.
    ///
    /// Two things must hold first, because a refusal is not always about size:
    ///
    /// * the pool served some smaller amount, so this really is a limit — a swap can also fail for
    ///   being too small to quote, and that fails at the opposite end;
    /// * the pool served no larger amount, which would prove it does not refuse by size at all.
    fn remember(&mut self, insert_at: usize, amount_in: &BigUint, outcome: Option<HopOutcome>) {
        let refused = outcome.is_none();
        self.outcome_by_amount
            .insert(insert_at, (amount_in.clone(), outcome));
        if !refused {
            return;
        }

        let was_served = |(_, outcome): &(BigUint, Option<HopOutcome>)| outcome.is_some();
        let served_below = self.outcome_by_amount[..insert_at]
            .iter()
            .any(was_served);
        let served_above = self.outcome_by_amount[insert_at + 1..]
            .iter()
            .any(was_served);
        if !served_below || served_above {
            return;
        }

        let lowest_refused = match self.refused_from.take() {
            Some(already_refused) if already_refused <= *amount_in => already_refused,
            _ => amount_in.clone(),
        };
        self.refused_from = Some(lowest_refused);
    }
}

/// Swaps already made, so a pool asked the same question twice is only simulated once.
///
/// **Only for swaps that read untouched component state.** Every answer here is kept for the whole
/// solve, which is sound exactly while nothing commits a swap back into the state being read. The
/// chunked water-fills do commit — they ask one pool the same question repeatedly and depend on a
/// worse answer each time as it is drained — so they simulate against their own overlay and must
/// not come through here.
///
/// A refusal is kept like any other answer: a pool that will not take an amount refuses every path
/// that reaches it with that amount. It is also read upwards — once a pool has refused, larger
/// amounts are refused without asking it again, on the terms [`AmountsSwapped::remember`] sets out.
/// That holds for every pass: it says what the pool would do, rather than approximating a number
/// it would return, so it holds for every pass, whatever [`SolvePass::may_interpolate`] says.
///
/// For a pass [`SolvePass::may_interpolate`] admits, an amount never asked for can be answered by
/// reading across the two nearest amounts that were. See [`SwapCache::interpolate`] for when that
/// is allowed and which way it errs.
struct SwapCache<'a> {
    by_direction: FxHashMap<PoolDirection<'a>, AmountsSwapped>,
}

impl<'a> SwapCache<'a> {
    fn new() -> Self {
        Self { by_direction: FxHashMap::default() }
    }

    /// What `direction` pays for `amount_in`.
    ///
    /// Answers from the amounts already asked of that pool where it can, reading across two of them
    /// when the asking pass allows it, and otherwise calls `simulate` and keeps the result. Every
    /// route through here is booked against the component and the pass, so the report separates
    /// what was simulated from what was reused and from what was read across.
    fn swap(
        &mut self,
        direction: PoolDirection<'a>,
        amount_in: &BigUint,
        pass: SolvePass,
        meter: &mut SolveMeter<'a>,
        simulate: impl FnOnce(&mut SolveMeter<'a>) -> Option<HopOutcome>,
    ) -> Option<HopOutcome> {
        let component_id = direction.component_id;
        let amounts_swapped = self
            .by_direction
            .entry(direction)
            .or_default();

        let insert_at = match amounts_swapped
            .outcome_by_amount
            .binary_search_by(|(amount, _)| amount.cmp(amount_in))
        {
            Ok(asked_before) => {
                meter.record_cache_hit(component_id, pass);
                return amounts_swapped.outcome_by_amount[asked_before]
                    .1
                    .clone();
            }
            Err(insert_at) => insert_at,
        };

        // Asking a pool for more than it has already turned down buys the same refusal again, and
        // on a `vm:` pool that is as expensive as a swap it would have served.
        if amounts_swapped.refuses(amount_in) {
            meter.record_refusal_without_calling(component_id, pass);
            return None;
        }

        if pass.may_interpolate() {
            if let Some(read_across) = Self::interpolate(amounts_swapped, insert_at, amount_in) {
                meter.record_interpolation(component_id, pass);
                return Some(read_across);
            }
        }

        // Only a simulated amount is kept. Keeping one that was itself read across would let the
        // error compound, each reading drifting further from the pool's own curve.
        let outcome = simulate(meter);
        amounts_swapped.remember(insert_at, amount_in, outcome.clone());
        outcome
    }

    /// What the pool would pay for `amount_in`, read across the amounts either side of it.
    ///
    /// Output against input is concave for a pool — each further unit in buys less out — so the
    /// straight line between two amounts runs below the pool's own curve. Reading across it
    /// therefore comes out a little low, never high, and a path can only lose a ranking it
    /// deserved rather than win one it did not.
    ///
    /// That only holds between two amounts. Past the largest one asked, the same line runs above
    /// the curve, because it carries a price the pool no longer offers — so those are simulated.
    ///
    /// The two amounts must also sit within [`INTERPOLATION_GAP_PERCENT`] of the one asked for.
    /// A wider gap is where the line drifts furthest from the curve, and the gap narrows on its
    /// own: a request this turns away is simulated, and that amount lands between the two,
    /// leaving a closer pair behind for the next pass.
    ///
    /// Gas takes the larger amount's figure, not the nearer one's. Gas does not climb smoothly — a
    /// crossed tick costs what it costs — and a swap never needs less gas than a smaller one, so
    /// the larger amount's figure is never under the truth. Ranking subtracts gas from output, so
    /// understating it here would overstate a path's net and let it take a place it had not
    /// earned, which is the one thing this must not do.
    fn interpolate(
        amounts_swapped: &AmountsSwapped,
        insert_at: usize,
        amount_in: &BigUint,
    ) -> Option<HopOutcome> {
        let (lower_amount, lower) = amounts_swapped
            .outcome_by_amount
            .get(insert_at.checked_sub(1)?)?;
        let (upper_amount, upper) = amounts_swapped
            .outcome_by_amount
            .get(insert_at)?;
        let (lower, upper) = (lower.as_ref()?, upper.as_ref()?);

        let amount_gap = upper_amount - lower_amount;
        if &amount_gap * 100u32 > amount_in * INTERPOLATION_GAP_PERCENT {
            return None;
        }
        // A pool paying less for more is not the concave curve this reads across.
        if upper.amount_out < lower.amount_out {
            return None;
        }

        let output_gap = &upper.amount_out - &lower.amount_out;
        let amount_past_lower = amount_in - lower_amount;
        let amount_out = &lower.amount_out + output_gap * amount_past_lower / amount_gap;
        Some(HopOutcome { amount_out, gas: upper.gas.clone() })
    }
}

/// What one path pays for the whole order.
#[derive(Clone)]
enum FullAmountOutcome {
    /// The path took the whole order, paying `output` and costing `gas` over its hops.
    Filled { output: BigUint, gas: BigUint },
    /// The path could not take the whole order. It still ranks by output, at zero, because a
    /// fraction of the order may suit it; it cannot be the single-path baseline.
    Unfilled,
}

/// The two orderings the full-amount pass produces, both holding indices into the path list it
/// ranked.
struct FullAmountRanking {
    /// Every path simulated, best output first. One that could not take the whole order ranks last
    /// at zero: it may still be worth a fraction in a split.
    by_output: Vec<usize>,
    /// The paths that filled the order, best output net of gas first. The single-path baseline is
    /// the first of these that builds into a route.
    by_output_net_gas: Vec<usize>,
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
    /// Every untouched-state swap discovery and ranking already made, handed on so the allocation
    /// passes that read untouched state do not repeat them.
    cache: SwapCache<'a>,
    /// What the swaps made so far cost, carried on so the whole solve reports as one.
    meter: SolveMeter<'a>,
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
    /// the pass can commit them into an overlay.
    fn simulate_step<'g>(
        path: &Path<'g, DepthAndPrice>,
        market: &MarketState,
        overlay: &FxHashMap<ComponentId, Box<dyn ProtocolSim>>,
        amount: BigUint,
        meter: &mut SolveMeter<'g>,
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
                .get_amount_out_metered(
                    component_id,
                    SolvePass::Chunking,
                    meter,
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
    fn activation_cost(ctx: &SplitContext, gas: &BigUint) -> BigInt {
        Self::gas_cost_in_token(gas, ctx.gas_price, ctx.token_prices, ctx.order.token_out())
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
    async fn setup<'a>(
        &self,
        graph: &'a TopologyGraph<DepthAndPrice>,
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

        let mut scored_paths = self.top_scored_paths(graph, order)?;
        let mut joined_paths = Vec::new();

        let timeout_ms = self.timeout.as_millis() as u64;
        let (market_view, gas_price) = read_market(&market, label).await?;

        // Bounded amount-aware discovery (see the discovery section below): union its
        // candidates ahead of the pre-ranked set, so connector/anchor routes (incl. the
        // native-ETH sentinel) survive the spot×depth truncation. Discovery failure is not
        // fatal — the pre-ranked set already guarantees a route.
        let anchor_tokens = derive_anchor_tokens(graph);

        // Discovery and ranking both swap against untouched state, so they share one cache: every
        // frontier edge discovery simulates is an answer ranking would otherwise pay for again.
        // It outlives setup because the allocation passes that read untouched state reuse it too.
        let mut cache = SwapCache::new();
        let mut meter = SolveMeter::new();

        let (discovered_paths, _) = discover_paths(
                graph,
                &market_view,
                order,
                &mut cache,
                &mut meter,
                CandidateSearchConfig {
                    query: &self.query,
                    max_candidates: MAX_DISCOVERY_CANDIDATES,
                    anchor_tokens: &anchor_tokens,
                    source_token: order.token_in(),
                    start: &start,
                    timeout_ms,
                },
            )
            .inspect_err(|e| {
                debug!(error = %e, "water-fill bounded discovery failed; using exhaustive candidates only")
            })
            .unwrap_or_default();

        trace!(discovered = discovered_paths.len(), "water-fill discovery");

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
        drop(market_view);

        let amount_in = order.amount().clone();
        let ranking = self.rank_at_full_amount(
            &joined_paths,
            &market_state,
            order,
            &gas_price,
            token_prices.as_ref(),
            &mut cache,
            &mut meter,
            start,
        );

        // No early exit on a missing single path: a split across thin components can fill an order
        // that no single path can, so the pass decides — it only errors when neither a
        // single path nor a split candidate fills the order.
        let ordered: Vec<Path<DepthAndPrice>> = ranking
            .by_output
            .iter()
            .map(|&path_ix| joined_paths[path_ix].clone())
            .collect();

        // Only the baseline is built into swaps: that is what copies a component and a pool state
        // per leg, and every other path would have thrown them away. Ranking already settled which
        // pays best net of gas, so this moves down the list only when the market cannot assemble
        // that path into a route at all.
        let best_single = ranking
            .by_output_net_gas
            .iter()
            .find_map(|&path_ix| {
                paths::simulate_pool_path(
                    &joined_paths[path_ix],
                    &market_state,
                    token_prices.as_ref(),
                    amount_in.clone(),
                )
                .ok()
            });

        debug!(
            candidate_paths = ordered.len(),
            elapsed_ms = start.elapsed().as_millis(),
            "water-fill discovery + full-amount ranking"
        );
        Ok(SetupResult {
            ordered,
            market: market_state,
            gas_price,
            best_single,
            token_prices,
            cache,
            meter,
        })
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
    /// Nothing here builds a route. Only the baseline the pass picks out of the ranking is worth
    /// copying a component and a pool state per leg.
    fn rank_at_full_amount<'a>(
        &self,
        paths: &[Path<'a, DepthAndPrice>],
        market: &MarketState,
        order: &Order,
        gas_price: &BigUint,
        token_prices: Option<&TokenGasPrices>,
        cache: &mut SwapCache<'a>,
        meter: &mut SolveMeter<'a>,
        start: Instant,
    ) -> FullAmountRanking {
        let order_amount = order.amount();
        let timeout_ms = self.timeout.as_millis() as u64;
        let mut outcomes_by_path: Vec<Option<FullAmountOutcome>> = vec![None; paths.len()];

        for (path_ix, path) in paths.iter().enumerate() {
            if start.elapsed().as_millis() as u64 > timeout_ms {
                break;
            }
            if path_reuses_component(path) {
                continue;
            }
            outcomes_by_path[path_ix] = Some(
                match simulate_path(
                    path,
                    market,
                    cache,
                    meter,
                    order_amount.clone(),
                    SolvePass::Ranking,
                ) {
                    Some(paid) => {
                        FullAmountOutcome::Filled { output: paid.amount_out, gas: paid.gas }
                    }
                    None => FullAmountOutcome::Unfilled,
                },
            );
        }

        rank_outcomes(outcomes_by_path, gas_price, token_prices, order.token_out())
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
        let start = Instant::now();
        if !order.is_sell() {
            return Err(AlgorithmError::ExactOutNotSupported);
        }

        let SetupResult {
            ordered,
            market,
            gas_price,
            best_single,
            token_prices,
            mut cache,
            mut meter,
        } = self
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
        // One coarse (20-chunk) water-fill over the component-disjoint paths feeds both the floor
        // split and the refined split, so run it once. It is cheap and always finishes, so
        // a tight timeout cannot cut it off while leaving the single path — a winning split
        // is never lost to the clock.
        let disjoint = Self::select_disjoint(ctx.ordered, self.max_paths);
        let coarse = (disjoint.len() >= 2)
            .then(|| self.disjoint_waterfill(&ctx, &disjoint, COARSE_CHUNKS, true, &mut meter))
            .flatten();
        if let Some(coarse) = coarse.as_deref() {
            // The 20-chunk floor split: exactly the coarse allocation.
            if let Some(c) = self.build_disjoint_legs(&ctx, &disjoint, coarse, &mut meter) {
                candidates.push(c);
            }
            // Finer allocation over the same active set (a bonus; a timeout may cut it off, and
            // then the floor stands).
            if let Some(c) =
                self.disjoint_refine(&ctx, &disjoint, coarse, FINE_CHUNKS, &mut cache, &mut meter)
            {
                candidates.push(c);
            }
        }
        // Fill-and-spill: a split that lets paths share a component and branch at an intermediate
        // token (a tree route), which the component-disjoint splits cannot express.
        if let Some(c) = self.fillspill_alloc(&ctx, FINE_CHUNKS, &mut cache, &mut meter) {
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
        meter.report(&market, start.elapsed().as_millis() as u64);
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
    /// this returns `None` and the pass falls back to the 20-chunk floor candidate.
    fn disjoint_refine<'g>(
        &self,
        ctx: &SplitContext<'_, 'g>,
        disjoint: &[usize],
        coarse: &[BigUint],
        fine_chunks: usize,
        cache: &mut SwapCache<'g>,
        meter: &mut SolveMeter<'g>,
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
        let fine = self.disjoint_waterfill(ctx, &active, fine_chunks, false, meter)?;

        // Exchange refinement. The fine water-fill quantizes each path to a whole chunk, so it can
        // sit up to one chunk off the equal-marginal optimum. Nudge flow between paths at sub-chunk
        // resolution, accepting only strictly-improving moves (never-lose).
        let refined = self.disjoint_exchange(ctx, &active, fine_chunks, fine, cache, meter);
        self.build_disjoint_legs(ctx, &active, &refined, meter)
    }

    /// Net output (gross output minus gas cost in output-token terms) of `path` simulated in
    /// isolation at `amount`. Component-disjoint paths never interfere, so an isolated
    /// re-simulation is exact. A zero amount means the path is dropped from the route: it
    /// yields no output and, since it is no longer swapped, no gas — so dropping a donor
    /// credits its saved gas automatically.
    fn path_net<'g>(
        ctx: &SplitContext<'_, 'g>,
        path: &Path<'g, DepthAndPrice>,
        amount: &BigUint,
        cache: &mut SwapCache<'g>,
        meter: &mut SolveMeter<'g>,
    ) -> Option<BigInt> {
        if amount.is_zero() {
            return Some(BigInt::zero());
        }
        let paid =
            simulate_path(path, ctx.market, cache, meter, amount.clone(), SolvePass::Exchange)?;
        let activation = Self::activation_cost(ctx, &paid.gas);
        Some(BigInt::from(paid.amount_out) - activation)
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
        ctx: &SplitContext<'_, 'g>,
        active: &[usize],
        fine_chunks: usize,
        alloc: Vec<BigUint>,
        cache: &mut SwapCache<'g>,
        meter: &mut SolveMeter<'g>,
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
            let Some(net) = Self::path_net(ctx, &ctx.ordered[path_idx], &cum[i], cache, meter)
            else {
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
                let Some(donor_net) =
                    Self::path_net(ctx, &ctx.ordered[active[donor]], &donor_amt, cache, meter)
                else {
                    continue;
                };
                sims += 1;
                for recipient in 0..path_count {
                    if recipient == donor || sims >= EXCHANGE_MAX_SIMS {
                        continue;
                    }
                    let recip_amt = &cum[recipient] + &delta;
                    let Some(recip_net) = Self::path_net(
                        ctx,
                        &ctx.ordered[active[recipient]],
                        &recip_amt,
                        cache,
                        meter,
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

    /// Simulates `amount` through `path`, reading and committing component states via `overrides`,
    /// and returns the allocation the route assembly consumes.
    fn allocation_commit<'g>(
        path: &Path<'g, DepthAndPrice>,
        market: &MarketState,
        overrides: &mut FxHashMap<ComponentId, Box<dyn ProtocolSim>>,
        amount: BigUint,
        flow_fraction: f64,
        meter: &mut SolveMeter<'g>,
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
                .get_amount_out_metered(
                    component_id,
                    SolvePass::Assembly,
                    meter,
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
    /// aligned by index; component-disjoint paths never interfere, so each leg is a real
    /// independent simulation.
    fn build_disjoint_legs<'g>(
        &self,
        ctx: &SplitContext<'_, 'g>,
        subset: &[usize],
        alloc: &[BigUint],
        meter: &mut SolveMeter<'g>,
    ) -> Option<SplitCandidate> {
        let amount_in = ctx.order.amount().clone();
        let mut allocations = Vec::new();
        for (i, &path_idx) in subset.iter().enumerate() {
            if alloc[i].is_zero() {
                continue;
            }
            // Fresh overrides per leg: legs are component-disjoint, but a component reused within
            // one path must still see its own first swap.
            let mut overrides: FxHashMap<ComponentId, Box<dyn ProtocolSim>> = FxHashMap::default();
            let allocation = Self::allocation_commit(
                &ctx.ordered[path_idx],
                ctx.market,
                &mut overrides,
                alloc[i].clone(),
                ratio(&alloc[i], &amount_in),
                meter,
            )?;
            allocations.push(allocation);
        }
        if allocations.is_empty() {
            return None;
        }
        Self::candidate_from_allocations(ctx, &allocations)
    }

    /// Incremental water-fill over a set of component-disjoint paths. Returns the amount allocated
    /// to each path in `subset` order. With `gate`, a path only activates when its first chunk
    /// covers its gas; without it, every path is eligible (used once the active set is fixed).
    fn disjoint_waterfill<'g>(
        &self,
        ctx: &SplitContext<'_, 'g>,
        subset: &[usize],
        num_chunks: usize,
        gate: bool,
        meter: &mut SolveMeter<'g>,
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

        let mut committed: Vec<FxHashMap<ComponentId, Box<dyn ProtocolSim>>> = (0..path_count)
            .map(|_| FxHashMap::default())
            .collect();
        let mut cum_in: Vec<BigUint> = vec![BigUint::zero(); path_count];
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
            if ctx.start.elapsed().as_millis() as u64 > timeout_ms {
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
                        &ctx.ordered[path_idx],
                        ctx.market,
                        &committed[i],
                        chunk.clone(),
                        meter,
                    );
                }
                let Some(step) = marginals[i].as_ref() else {
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
            cum_in[best_i] += &chunk;
            activated[best_i] = true;
        }
        Some(cum_in)
    }

    /// Selects fill-and-spill candidates: the top full-amount paths plus the best first-chunk
    /// marginal probes. The probe is what makes intermediate-token splits (tree routes) reachable:
    /// the extra path often ranks poorly at full size but wins on the margin.
    fn select_shared_candidates<'g>(
        &self,
        ctx: &SplitContext<'_, 'g>,
        cache: &mut SwapCache<'g>,
        meter: &mut SolveMeter<'g>,
    ) -> Vec<usize> {
        let mut candidates: Vec<usize> = (0..ctx.ordered.len().min(SHARED_FULL_PATHS)).collect();
        let first_chunk = ctx.order.amount() / COARSE_CHUNKS;
        if first_chunk.is_zero() {
            return candidates;
        }
        let timeout_ms = self.timeout.as_millis() as u64;
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
            // Nothing is committed yet, so these probes read untouched state and go through the
            // cache like every other swap that does.
            let Some(probe) = simulate_path(
                path,
                ctx.market,
                cache,
                meter,
                first_chunk.clone(),
                SolvePass::SetSelection,
            ) else {
                continue;
            };
            let activation = Self::activation_cost(ctx, &probe.gas);
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
        ctx: &SplitContext<'_, 'g>,
        fine_chunks: usize,
        cache: &mut SwapCache<'g>,
        meter: &mut SolveMeter<'g>,
    ) -> Option<SplitCandidate> {
        let candidates = self.select_shared_candidates(ctx, cache, meter);
        if candidates.len() < 2 {
            return None;
        }

        // Phase 1: coarse gated pass to choose the active candidate set.
        let (coarse_counts, _) =
            self.fillspill_waterfill(ctx, &candidates, COARSE_CHUNKS, true, meter)?;
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
        let (_, schedule) = self.fillspill_waterfill(ctx, &active, fine_chunks, false, meter)?;
        if schedule.is_empty() {
            return None;
        }

        self.build_fillspill_route(ctx, &active, &schedule, meter)
    }

    /// Incremental fill-and-spill water-fill over a single shared overlay. Returns the chunk count
    /// each candidate received and the ordered commit schedule of `(active_index, chunk_amount)`.
    #[allow(clippy::type_complexity)]
    fn fillspill_waterfill<'g>(
        &self,
        ctx: &SplitContext<'_, 'g>,
        subset: &[usize],
        num_chunks: usize,
        gate: bool,
        meter: &mut SolveMeter<'g>,
    ) -> Option<(Vec<usize>, Vec<(usize, BigUint)>)> {
        let amount_in = ctx.order.amount().clone();
        let num_chunks = num_chunks.max(1);
        let base_chunk = &amount_in / num_chunks;
        if base_chunk.is_zero() {
            return None;
        }
        let remainder = &amount_in - &base_chunk * num_chunks;
        let timeout_ms = self.timeout.as_millis() as u64;

        let mut overlay: FxHashMap<ComponentId, Box<dyn ProtocolSim>> = FxHashMap::default();
        let mut activated: Vec<bool> = vec![!gate; subset.len()];
        let mut active_count = if gate { 0 } else { subset.len() };
        let mut counts: Vec<usize> = vec![0; subset.len()];
        let mut schedule: Vec<(usize, BigUint)> = Vec::with_capacity(num_chunks);
        // Marginals thrown away because the winning chunk moved a pool on that candidate, and the
        // hops it crosses before that pool — the part of each re-simulation that repeats itself.
        let mut forgotten_marginals = 0usize;
        let mut hops_before_moved_pool = 0usize;

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
            if ctx.start.elapsed().as_millis() as u64 > timeout_ms {
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
                        &ctx.ordered[path_idx],
                        ctx.market,
                        &overlay,
                        chunk.clone(),
                        meter,
                    );
                }
                let Some(step) = marginals[i].as_ref() else {
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

            {
                let pools_moved: FxHashSet<&ComponentId> = step
                    .new_states
                    .iter()
                    .map(|(id, _)| id)
                    .collect();
                for (i, &path_idx) in subset.iter().enumerate() {
                    let Some(first_moved_hop) = ctx.ordered[path_idx]
                        .edge_iter()
                        .iter()
                        .position(|e| pools_moved.contains(&e.component_id))
                    else {
                        continue;
                    };
                    // Only a marginal that was actually held counts as thrown away. The winner's
                    // was taken just above, and a candidate the activation gate keeps skipping
                    // never refills one, so counting either would overstate the repeated work.
                    if marginals[i].take().is_some() {
                        forgotten_marginals += 1;
                        hops_before_moved_pool += first_moved_hop;
                    }
                }
            }

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

        debug!(
            chunks = num_chunks,
            candidates = subset.len(),
            forgotten_marginals,
            hops_before_moved_pool,
            "water-fill fill-and-spill re-simulation"
        );
        Some((counts, schedule))
    }

    /// Rebuilds the fill-and-spill result as one leg per active path at its total allocated
    /// amount, committed sequentially (largest allocation first) against a shared overlay — the
    /// same execution model the router applies on-chain.
    fn build_fillspill_route<'g>(
        &self,
        ctx: &SplitContext<'_, 'g>,
        active: &[usize],
        schedule: &[(usize, BigUint)],
        meter: &mut SolveMeter<'g>,
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

        let mut overrides: FxHashMap<ComponentId, Box<dyn ProtocolSim>> = FxHashMap::default();
        let mut allocations = Vec::new();
        for i in execution_order {
            let allocation = Self::allocation_commit(
                &ctx.ordered[active[i]],
                ctx.market,
                &mut overrides,
                cand_in[i].clone(),
                ratio(&cand_in[i], &amount_in),
                meter,
            )?;
            allocations.push(allocation);
        }
        Self::candidate_from_allocations(ctx, &allocations)
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
fn simulate_hop<'a>(
    market: &MarketState,
    component_id: &'a ComponentId,
    address_in: &Address,
    address_out: &Address,
    amount_in: &BigUint,
    pass: SolvePass,
    meter: &mut SolveMeter<'a>,
) -> Option<HopOutcome> {
    let token_in = market.get_token(address_in)?;
    let token_out = market.get_token(address_out)?;
    let state = market.get_simulation_state(component_id)?;
    let result = state
        .get_amount_out_metered(component_id, pass, meter, amount_in.clone(), token_in, token_out)
        .ok()?;
    Some(HopOutcome { amount_out: result.amount, gas: result.gas })
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
    meter: &mut SolveMeter<'a>,
    amount_in: BigUint,
    pass: SolvePass,
) -> Option<HopOutcome> {
    // What the next hop swaps; once the last one is done, what the path pays out.
    let mut hop_amount_in = amount_in;
    let mut path_gas = BigUint::zero();

    for (address_in, edge, address_out) in path.iter() {
        let direction = PoolDirection { component_id: &edge.component_id, address_in, address_out };
        let hop = cache.swap(direction, &hop_amount_in, pass, meter, |meter| {
            simulate_hop(
                market,
                &edge.component_id,
                address_in,
                address_out,
                &hop_amount_in,
                pass,
                meter,
            )
        })?;
        hop_amount_in = hop.amount_out;
        path_gas += hop.gas;
    }

    Some(HopOutcome { amount_out: hop_amount_in, gas: path_gas })
}

/// Turns the full-amount pass's per-path outcomes into the two orderings the pass consumes. Both
/// are built in path order and sorted stably, so paths paying the same amount keep the order they
/// were enumerated in. A path with no outcome never ran — the pass timed out before reaching it,
/// or it crossed a pool twice — and appears in neither ordering.
fn rank_outcomes(
    outcomes_by_path: Vec<Option<FullAmountOutcome>>,
    gas_price: &BigUint,
    token_prices: Option<&TokenGasPrices>,
    token_out: &Address,
) -> FullAmountRanking {
    let mut by_output: Vec<(usize, BigUint)> = Vec::with_capacity(outcomes_by_path.len());
    let mut by_output_net_gas: Vec<(usize, BigInt)> = Vec::new();

    for (path_ix, outcome) in outcomes_by_path.into_iter().enumerate() {
        let Some(outcome) = outcome else {
            continue;
        };
        let output = match outcome {
            FullAmountOutcome::Filled { output, gas } => {
                let gas_cost =
                    WaterFillAlgorithm::gas_cost_in_token(&gas, gas_price, token_prices, token_out);
                let net_output = match gas_cost {
                    Some(gas_cost) => BigInt::from(output.clone()) - BigInt::from(gas_cost),
                    None => BigInt::from(output.clone()),
                };
                by_output_net_gas.push((path_ix, net_output));
                output
            }
            FullAmountOutcome::Unfilled => BigUint::zero(),
        };
        by_output.push((path_ix, output));
    }

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
    /// The same hop bounds and connector set every other route search runs under.
    query: &'a GraphQueryFilter,
    max_candidates: usize,
    anchor_tokens: &'a FxHashSet<Address>,
    source_token: &'a Address,
    start: &'a Instant,
    timeout_ms: u64,
}

fn timed_out(start: &Instant, timeout_ms: u64) -> bool {
    start.elapsed().as_millis() as u64 > timeout_ms
}

/// Runs the bounded discovery and returns the candidate paths plus their `(index, full-amount
/// gross output)` ranking, best first.
fn discover_paths<'a, W>(
    graph: &'a TopologyGraph<W>,
    market: &MarketDataView<'_>,
    order: &Order,
    cache: &mut SwapCache<'a>,
    meter: &mut SolveMeter<'a>,
    cfg: CandidateSearchConfig<'_>,
) -> Result<CandidatePathSet<'a, W>, AlgorithmError>
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
        if timed_out(cfg.start, cfg.timeout_ms) || frontier.is_empty() {
            break;
        }
        let mut next_by_node: FxHashMap<NodeIndex, Vec<CandidatePathState<'a, W>>> =
            FxHashMap::default();
        for state in frontier {
            if state.node == to_idx && from_idx != to_idx {
                continue;
            }

            expand_candidate_state(
                graph,
                market,
                &cfg,
                cache,
                meter,
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
    graph: &'a TopologyGraph<W>,
    market: &MarketDataView<'_>,
    cfg: &CandidateSearchConfig<'_>,
    cache: &mut SwapCache<'a>,
    meter: &mut SolveMeter<'a>,
    target: NodeIndex,
    state: CandidatePathState<'a, W>,
    found: &mut Vec<(Path<'a, W>, BigUint)>,
    next_by_node: &mut FxHashMap<NodeIndex, Vec<CandidatePathState<'a, W>>>,
) where
    W: Clone,
{
    let edges = candidate_edges_for_state(graph, market, cfg, cache, meter, target, &state);
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
    graph: &'a TopologyGraph<W>,
    market: &MarketDataView<'_>,
    cfg: &CandidateSearchConfig<'_>,
    cache: &mut SwapCache<'a>,
    meter: &mut SolveMeter<'a>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
) -> Vec<ScoredEdge<'a, W>> {
    let mut preferred =
        score_candidate_edges(graph, market, cfg, cache, meter, target, state, true);
    if preferred.is_empty() {
        preferred = score_candidate_edges(graph, market, cfg, cache, meter, target, state, false);
    }
    select_candidate_edges(preferred, CANDIDATE_EDGES_PER_STATE)
}

#[allow(clippy::too_many_arguments)]
fn score_candidate_edges<'a, W>(
    graph: &'a TopologyGraph<W>,
    market: &MarketDataView<'_>,
    cfg: &CandidateSearchConfig<'_>,
    cache: &mut SwapCache<'a>,
    meter: &mut SolveMeter<'a>,
    target: NodeIndex,
    state: &CandidatePathState<'a, W>,
    preferred_only: bool,
) -> Vec<ScoredEdge<'a, W>> {
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
            let Some(hop) =
                cache.swap(direction, &state.amount_out, SolvePass::Discovery, meter, |meter| {
                    simulate_edge(market, &state.amount_out, address_in, pool, address_out, meter)
                })
            else {
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

fn simulate_edge<'a, W>(
    market: &MarketDataView<'_>,
    amount: &BigUint,
    token_in_addr: &Address,
    edge: &'a EdgeData<W>,
    token_out_addr: &Address,
    meter: &mut SolveMeter<'a>,
) -> Option<HopOutcome> {
    let token_in = market.get_token(token_in_addr)?;
    let token_out = market.get_token(token_out_addr)?;
    let state = market.get_simulation_state(&edge.component_id)?;
    let result = state
        .get_amount_out_metered(
            &edge.component_id,
            SolvePass::Discovery,
            meter,
            amount.clone(),
            token_in,
            token_out,
        )
        .ok()?;
    Some(HopOutcome { amount_out: result.amount, gas: result.gas })
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
) -> Result<CandidatePathSet<'a, W>, AlgorithmError> {
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
    Ok((paths, scores))
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
        let (paths, scores) = discover_paths(
            graph_manager.graph(),
            &view,
            &order,
            &mut SwapCache::new(),
            &mut SolveMeter::new(),
            CandidateSearchConfig {
                query: &GraphQueryFilter { min_hops: 1, max_hops: 3, connector_tokens: None },
                max_candidates: 128,
                anchor_tokens: &FxHashSet::default(),
                source_token: order.token_in(),
                start: &start,
                timeout_ms: 2000,
            },
        )
        .expect("discovery finds candidates");

        assert_eq!(paths.len(), 2, "both parallel components should be discovered");
        // Scores are (path index, full-amount gross output), best first: the deeper component wins.
        let best_path = &paths[scores[0].0];
        assert_eq!(
            best_path.edge_iter()[0].component_id,
            "z_strong_link_weth",
            "discovery should rank by simulated output, not topology or edge weights",
        );
    }

    // ==================== Reading across known amounts ====================

    fn hop(amount_out: u64, gas: u64) -> HopOutcome {
        HopOutcome { amount_out: BigUint::from(amount_out), gas: BigUint::from(gas) }
    }

    /// A cache holding `amounts` for one pool direction, with no refusal point recorded.
    fn cache_holding(amounts: Vec<(u64, Option<HopOutcome>)>) -> AmountsSwapped {
        AmountsSwapped {
            outcome_by_amount: amounts
                .into_iter()
                .map(|(amount, outcome)| (BigUint::from(amount), outcome))
                .collect(),
            refused_from: None,
        }
    }

    /// Where `amount` would be inserted into a cache's ascending amounts.
    fn insert_at(swapped: &AmountsSwapped, amount: &BigUint) -> usize {
        swapped
            .outcome_by_amount
            .binary_search_by(|(known, _)| known.cmp(amount))
            .expect_err("amount must not already be recorded")
    }

    fn read_across(swapped: &AmountsSwapped, amount: u64) -> Option<HopOutcome> {
        let amount = BigUint::from(amount);
        SwapCache::interpolate(swapped, insert_at(swapped, &amount), &amount)
    }

    /// Halfway between two amounts reads back halfway between their outputs.
    #[test]
    fn test_interpolate_reads_across_two_amounts() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(2080, 90)))]);

        let across = read_across(&swapped, 1020).expect("bracketed and inside the gap");

        assert_eq!(across.amount_out, BigUint::from(2040u64));
    }

    /// Gas comes from the larger amount, never the nearer one: understating gas would overstate a
    /// path's output net of gas, which is the one direction reading across must not err in.
    #[test]
    fn test_interpolate_takes_gas_from_the_larger_amount() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(2080, 90)))]);

        let nearer_the_lower = read_across(&swapped, 1001).expect("bracketed and inside the gap");

        assert_eq!(nearer_the_lower.gas, BigUint::from(90u64));
    }

    /// Amounts further apart than `INTERPOLATION_GAP_PERCENT` are left to the pool.
    #[test]
    fn test_interpolate_declines_a_wide_gap() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1500, Some(hop(2600, 90)))]);

        assert!(read_across(&swapped, 1200).is_none());
    }

    /// Above every amount asked, the straight line carries a price the pool no longer offers, so
    /// there is nothing to read across.
    #[test]
    fn test_interpolate_declines_above_the_largest_amount() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(2080, 90)))]);

        assert!(read_across(&swapped, 1050).is_none());
    }

    /// A pool paying less for more is not the curve this reads across.
    #[test]
    fn test_interpolate_declines_when_output_falls() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, Some(hop(1900, 90)))]);

        assert!(read_across(&swapped, 1020).is_none());
    }

    /// A refused amount either side leaves nothing to read across.
    #[test]
    fn test_interpolate_declines_across_a_refusal() {
        let swapped = cache_holding(vec![(1000, Some(hop(2000, 50))), (1040, None)]);

        assert!(read_across(&swapped, 1020).is_none());
    }

    // ==================== Refusals reaching upwards ====================

    fn remember(swapped: &mut AmountsSwapped, amount: u64, outcome: Option<HopOutcome>) {
        let amount = BigUint::from(amount);
        let at = insert_at(swapped, &amount);
        swapped.remember(at, &amount, outcome);
    }

    /// A refusal above an amount the pool served is taken to refuse everything larger.
    #[test]
    fn test_refusal_above_a_served_amount_reaches_upwards() {
        let mut swapped = cache_holding(vec![(1000, Some(hop(2000, 50)))]);

        remember(&mut swapped, 2000, None);

        assert!(swapped.refuses(&BigUint::from(2000u64)));
        assert!(swapped.refuses(&BigUint::from(5000u64)));
        assert!(!swapped.refuses(&BigUint::from(1500u64)));
    }

    /// A refusal with nothing served below it may be an amount too small to quote rather than a
    /// limit, so it stands only for itself.
    #[test]
    fn test_refusal_with_nothing_served_below_stands_alone() {
        let mut swapped = cache_holding(vec![]);

        remember(&mut swapped, 1000, None);

        assert!(!swapped.refuses(&BigUint::from(5000u64)));
    }

    /// A larger amount the pool did serve says it does not refuse upwards at all.
    #[test]
    fn test_refusal_below_a_served_amount_does_not_reach_upwards() {
        let mut swapped =
            cache_holding(vec![(1000, Some(hop(2000, 50))), (3000, Some(hop(5000, 50)))]);

        remember(&mut swapped, 2000, None);

        assert!(!swapped.refuses(&BigUint::from(4000u64)));
    }

    /// A second, lower refusal moves the point everything above is refused from down to it.
    #[test]
    fn test_lower_refusal_moves_the_refusal_point_down() {
        let mut swapped = cache_holding(vec![(1000, Some(hop(2000, 50)))]);

        remember(&mut swapped, 3000, None);
        remember(&mut swapped, 2000, None);

        assert!(swapped.refuses(&BigUint::from(2000u64)));
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
