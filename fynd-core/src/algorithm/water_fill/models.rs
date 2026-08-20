use std::time::{Duration, Instant};

use num_bigint::{BigInt, BigUint};
use petgraph::graph::NodeIndex;
use rustc_hash::FxHashSet;
use tycho_simulation::tycho_common::{models::Address, simulation::protocol_sim::ProtocolSim};

use crate::{
    algorithm::{
        most_liquid::DepthAndPrice,
        sim_meter,
        swap_cache::{SwapCache, SwapResult},
        WaterFillAlgorithm,
    },
    derived::TokenGasPrices,
    feed::market_data::{MarketDataView, MarketState},
    graph::{EdgeData, GraphQueryFilter, Path, TopologyGraph},
    types::RouteResult,
    ComponentId, Order, Route,
};

/// A fully-built split candidate: the assembled route plus its summed gross output and gas.
pub struct SplitCandidate {
    pub route: Route,
    pub gross: BigUint,
    pub gas: BigUint,
}

impl SplitCandidate {
    /// Net output in output-token terms (gross minus gas cost).
    pub(crate) fn net(&self, input: &SolveInput<'_, '_>) -> BigInt {
        let cost = WaterFillAlgorithm::gas_cost_in_token(
            &self.gas,
            &input.gas_price,
            input.token_prices.as_ref(),
            input.order.token_out(),
        );
        match cost {
            Some(c) => BigInt::from(self.gross.clone()) - BigInt::from(c),
            None => BigInt::from(self.gross.clone()),
        }
    }
}

/// A candidate reallocation in the exchange-refinement pass: shift one `delta` of input from the
/// over-allocated `donor` to the under-allocated `recipient`, carrying the two paths' recomputed
/// net outputs and the resulting gain in summed net output.
pub struct ExchangeMove {
    pub donor: usize,
    pub recipient: usize,
    pub donor_net: BigInt,
    pub recip_net: BigInt,
    pub gain: BigInt,
}

/// One simulated traversal of a path, with the resulting per-component states so they can be
/// committed.
pub struct StepResult {
    pub amount_out: BigUint,
    pub gas: BigUint,
    pub new_states: Vec<(ComponentId, Box<dyn ProtocolSim>)>,
}

/// What one path pays for the whole order.
#[derive(Clone)]
pub enum FullAmountOutcome {
    /// The path took the whole order, paying what the swap reports over its hops.
    Filled(SwapResult),
    /// The path could not take the whole order. It still ranks by output, at zero, because a
    /// fraction of the order may suit it; it cannot be the single-path baseline.
    Unfilled,
}

/// The two orderings the full-amount pass produces, both holding indices into the path list it
/// ranked.
pub struct FullAmountRanking {
    /// Every path simulated, best output net of gas first — the same measure the baseline and the
    /// finished split are judged on, so a long route does not rank above a short one on an output
    /// its extra swaps hand straight back. One that could not take the whole order ranks last: it
    /// may still be worth a fraction in a split.
    pub by_output: Vec<usize>,
    /// The paths that filled the order, best output net of gas first. The single-path baseline is
    /// the first of these that builds into a route.
    pub by_output_net_gas: Vec<usize>,
}

/// What a solve reads: the ranked candidate paths, the market snapshot they touch, gas pricing,
/// and the order under a single solve clock.
///
/// Owns the market and pricing rather than borrowing them, so setup can hand the whole thing back
/// and every allocation pass takes one argument instead of the same six.
pub struct SolveInput<'o, 'g> {
    /// Candidate paths, best full-amount output net of gas first.
    pub ordered: Vec<Path<'g, DepthAndPrice>>,
    pub market: MarketState,
    pub gas_price: BigUint,
    pub token_prices: Option<TokenGasPrices>,
    pub order: &'o Order,
    pub deadline: Deadline,
}

/// Output of the shared setup pass.
pub struct SetupResult<'o, 'g> {
    /// Everything the allocation passes read.
    pub input: SolveInput<'o, 'g>,
    /// The best single path, when one fills the order — the bar every split has to beat.
    pub best_single: Option<RouteResult>,
    /// Every untouched-state swap discovery and ranking already made, handed on so the allocation
    /// passes that read untouched state do not repeat them.
    pub cache: SwapCache<'g>,
}

type RankedPathScores = Vec<(usize, BigInt)>;

#[derive(Clone)]
pub struct CandidatePathState<'a, W> {
    pub node: NodeIndex,
    pub path: Path<'a, W>,
    pub amount_out: BigUint,
}

pub struct ScoredEdge<'a, W> {
    pub target: NodeIndex,
    pub edge: &'a EdgeData<W>,
    pub amount_out: BigUint,
    pub priority: u8,
}

/// Parameters for one bounded candidate-discovery run.
#[derive(Clone, Copy)]
pub struct CandidateSearchConfig<'a> {
    /// The same hop bounds and connector set every other route search runs under.
    pub query: &'a GraphQueryFilter,
    pub max_candidates: usize,
    pub anchor_tokens: &'a FxHashSet<Address>,
    pub source_token: &'a Address,
    pub deadline: Deadline,
}

/// What one bounded discovery run walks with: the graph, the market it prices against, the bounds
/// it runs under, and the cache its swaps go through.
///
/// Held together because every step of the walk needs all four; passing them separately is what
/// pushed these signatures past what the reader can hold.
pub struct Discovery<'a, 'r, W> {
    pub graph: &'a TopologyGraph<W>,
    pub market: &'r MarketDataView<'r>,
    pub cfg: &'r CandidateSearchConfig<'r>,
    pub cache: &'r mut SwapCache<'a>,
}

/// The stage of a solve a swap was asked for.
///
/// Recorded against every swap so the report can say how much of a solve sits where answers can be
/// reused, and how much is in the passes that read state they have committed to and can never come
/// through the cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SolveStage {
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

impl SolveStage {
    /// What the simulation report calls this pass.
    pub(crate) fn label(self) -> sim_meter::StageLabel {
        match self {
            SolveStage::Discovery => "discovery",
            SolveStage::Ranking => "ranking",
            SolveStage::SetSelection => "set-selection",
            SolveStage::Exchange => "exchange",
            SolveStage::Chunking => "chunking",
            SolveStage::Assembly => "assembly",
        }
    }

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
            SolveStage::Ranking | SolveStage::SetSelection => true,
            SolveStage::Discovery |
            SolveStage::Exchange |
            SolveStage::Chunking |
            SolveStage::Assembly => false,
        }
    }
}

/// When a solve must stop, however many passes it has left.
///
/// One value rather than a start instant and a budget carried side by side: every pass asks the
/// same question, and asking it of two fields is how they drift apart.
#[derive(Clone, Copy)]
pub struct Deadline {
    pub start: Instant,
    pub timeout: Duration,
}

impl Deadline {
    pub(crate) fn new(start: Instant, timeout: Duration) -> Self {
        Self { start, timeout }
    }

    /// Whether the solve has run past its budget.
    pub(crate) fn expired(&self) -> bool {
        self.start.elapsed() > self.timeout
    }

    /// How long the solve has been running, for the lines that report it.
    pub(crate) fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}
