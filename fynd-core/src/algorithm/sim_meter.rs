//! Measurement of the simulation work one solve does.
//!
//! Simulating a swap is the dominant cost of solving, and that cost varies by orders of magnitude
//! between protocols — a constant-product pool is arithmetic, a `vm:*` pool runs EVM bytecode. This
//! records what was asked of which component, so a solve can say where its time went.
//!
//! Callers go through [`MeteredProtocolSim::get_amount_out_metered`], which wraps the panic guard
//! in [`super::sim_guard`] rather than replacing it. Only `water_fill` uses it; every other caller
//! still takes the bare guard, so nothing outside a solve is counted.

use std::{
    cmp::Reverse,
    time::{Duration, Instant},
};

use metrics::counter;
use num_bigint::BigUint;
use rustc_hash::FxHashMap;
use tracing::debug;
use tycho_simulation::tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
};

use super::sim_guard::GuardedProtocolSim;
use crate::{feed::market_data::MarketState, types::ComponentId};

/// The pass a swap was asked for.
///
/// Recorded against every swap so the report can say how much of a solve sits where answers can be
/// reused, and how much is in the passes that read state they have committed to and can never come
/// through the cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SwapCaller {
    /// Bounded discovery expanding its frontier.
    Discovery,
    /// Ranking every candidate path at the full order amount.
    Ranking,
    /// Choosing the fill-and-spill candidate set with first-chunk probes.
    Probes,
    /// Exchange refinement re-pricing a path on its own.
    Exchange,
    /// The chunked water-fills, which read what they have committed and never reach the cache.
    Chunking,
    /// Building the legs of a route that will be returned.
    Assembly,
}

impl SwapCaller {
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
            SwapCaller::Ranking | SwapCaller::Probes => true,
            SwapCaller::Discovery |
            SwapCaller::Exchange |
            SwapCaller::Chunking |
            SwapCaller::Assembly => false,
        }
    }

    fn name(self) -> &'static str {
        match self {
            SwapCaller::Discovery => "discovery",
            SwapCaller::Ranking => "ranking",
            SwapCaller::Probes => "probes",
            SwapCaller::Exchange => "exchange",
            SwapCaller::Chunking => "chunking",
            SwapCaller::Assembly => "assembly",
        }
    }
}

/// The `get_amount_out` calls one component served over a solve.
#[derive(Default, Clone, Copy)]
struct ComponentCalls {
    /// Calls made against this component.
    calls: u64,
    /// Of those, the ones that came back an error: the pool could not quote the swap, or its math
    /// panicked and [`GuardedProtocolSim`] turned that into an error.
    failed: u64,
    /// Swaps the cache answered instead, so no call was made at all.
    cache_hits: u64,
    /// Swaps answered by reading across two nearby amounts the pool had already been asked, so
    /// again no call was made. Only the passes that settle which paths get split, and the answer
    /// runs a little low.
    interpolated: u64,
    /// Swaps refused without calling, because the pool had already refused a smaller amount.
    refused_without_calling: u64,
    /// Time spent inside the calls that were made.
    call_time: Duration,
}

impl ComponentCalls {
    fn add(&mut self, other: &ComponentCalls) {
        self.calls += other.calls;
        self.failed += other.failed;
        self.cache_hits += other.cache_hits;
        self.interpolated += other.interpolated;
        self.refused_without_calling += other.refused_without_calling;
        self.call_time += other.call_time;
    }
}

/// Simulation work done while solving one order.
///
/// Counted per component, not per protocol: which protocol a component belongs to is known only to
/// the market, so it is resolved once in [`SolveMeter::report`] rather than looked up on every
/// swap. Resolving it there also names every `vm:*` protocol individually, where reading it off
/// the concrete state type would collapse them into one.
///
/// Component ids are borrowed from the graph, which outlives the solve, so recording a swap
/// allocates nothing.
pub(crate) struct SolveMeter<'a> {
    by_component_and_pass: FxHashMap<(&'a ComponentId, SwapCaller), ComponentCalls>,
}

impl<'a> SolveMeter<'a> {
    pub(crate) fn new() -> Self {
        Self { by_component_and_pass: FxHashMap::default() }
    }

    /// Records one `get_amount_out` call and how long it took.
    fn record_call(
        &mut self,
        component_id: &'a ComponentId,
        caller: SwapCaller,
        call_time: Duration,
        failed: bool,
    ) {
        let counts = self.counts(component_id, caller);
        counts.calls += 1;
        counts.call_time += call_time;
        if failed {
            counts.failed += 1;
        }
    }

    /// Records a swap the cache answered, so no call was made.
    pub(crate) fn record_cache_hit(&mut self, component_id: &'a ComponentId, caller: SwapCaller) {
        self.counts(component_id, caller).cache_hits += 1;
    }

    /// Records a swap answered by reading across two nearby amounts, so no call was made.
    pub(crate) fn record_interpolation(
        &mut self,
        component_id: &'a ComponentId,
        caller: SwapCaller,
    ) {
        self.counts(component_id, caller).interpolated += 1;
    }

    /// Records a swap refused on the strength of a smaller amount the pool already refused.
    pub(crate) fn record_refusal_without_calling(
        &mut self,
        component_id: &'a ComponentId,
        caller: SwapCaller,
    ) {
        self.counts(component_id, caller)
            .refused_without_calling += 1;
    }

    fn counts(&mut self, component_id: &'a ComponentId, caller: SwapCaller) -> &mut ComponentCalls {
        self.by_component_and_pass
            .entry((component_id, caller))
            .or_default()
    }

    /// Totals over every component.
    fn totals(&self) -> ComponentCalls {
        let mut totals = ComponentCalls::default();
        for counts in self.by_component_and_pass.values() {
            totals.add(counts);
        }
        totals
    }

    /// Writes the line saying what the solve asked of which protocol, and feeds the same counts to
    /// the metrics recorder. A component the market no longer holds is reported under `unknown`
    /// rather than dropped, so the totals still add up.
    pub(crate) fn report(&self, market: &MarketState, solve_time_ms: u64) {
        let mut by_protocol: FxHashMap<&str, ComponentCalls> = FxHashMap::default();
        for ((component_id, _), counts) in &self.by_component_and_pass {
            let protocol = market
                .get_component(component_id)
                .map_or("unknown", |component| component.protocol_system.as_str());
            by_protocol
                .entry(protocol)
                .or_default()
                .add(counts);
        }

        let mut costliest_first: Vec<(&str, ComponentCalls)> = by_protocol.into_iter().collect();
        costliest_first
            .sort_unstable_by_key(|(protocol, counts)| (Reverse(counts.call_time), *protocol));

        for (protocol, counts) in &costliest_first {
            counter!("water_fill.get_amount_out_calls", "protocol" => protocol.to_string())
                .increment(counts.calls);
            counter!("water_fill.failed_calls", "protocol" => protocol.to_string())
                .increment(counts.failed);
            counter!("water_fill.cache_hits", "protocol" => protocol.to_string())
                .increment(counts.cache_hits);
            counter!("water_fill.interpolated", "protocol" => protocol.to_string())
                .increment(counts.interpolated);
            counter!("water_fill.refused_without_calling", "protocol" => protocol.to_string())
                .increment(counts.refused_without_calling);
        }

        let per_protocol = costliest_first
            .iter()
            .map(|(protocol, counts)| {
                format!(
                    "{protocol}: {} calls ({} failed) in {:.1}ms, {} cache hits, {} interpolated, \
                     {} refused without calling",
                    counts.calls,
                    counts.failed,
                    counts.call_time.as_secs_f64() * 1000.0,
                    counts.cache_hits,
                    counts.interpolated,
                    counts.refused_without_calling,
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");

        let mut by_pass: FxHashMap<SwapCaller, ComponentCalls> = FxHashMap::default();
        let mut components: FxHashMap<&ComponentId, ()> = FxHashMap::default();
        for ((component_id, caller), counts) in &self.by_component_and_pass {
            by_pass
                .entry(*caller)
                .or_default()
                .add(counts);
            components.insert(component_id, ());
        }
        let mut passes: Vec<(SwapCaller, ComponentCalls)> = by_pass.into_iter().collect();
        passes.sort_unstable_by_key(|(_, counts)| Reverse(counts.call_time));
        let per_pass = passes
            .iter()
            .map(|(caller, counts)| {
                format!(
                    "{}: {} calls in {:.1}ms, {} answered without calling",
                    caller.name(),
                    counts.calls,
                    counts.call_time.as_secs_f64() * 1000.0,
                    counts.cache_hits + counts.interpolated + counts.refused_without_calling,
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        debug!(solve_time_ms, "water-fill simulation by pass: {per_pass}");

        let totals = self.totals();
        debug!(
            solve_time_ms,
            components = components.len(),
            get_amount_out_calls = totals.calls,
            failed_calls = totals.failed,
            cache_hits = totals.cache_hits,
            interpolated = totals.interpolated,
            refused_without_calling = totals.refused_without_calling,
            call_time_ms = totals.call_time.as_secs_f64() * 1000.0,
            "water-fill simulation cost: {per_protocol}",
        );
    }
}

/// Extension trait adding metered, panic-guarded simulation calls to every [`ProtocolSim`].
///
/// Wraps [`GuardedProtocolSim::get_amount_out_guarded`] and books the call against the component
/// that served it, which the guard alone cannot do — it sees only the state and the two tokens.
pub(crate) trait MeteredProtocolSim {
    /// Calls the panic-guarded `get_amount_out` and records it against `component_id`.
    fn get_amount_out_metered<'a>(
        &self,
        component_id: &'a ComponentId,
        caller: SwapCaller,
        meter: &mut SolveMeter<'a>,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError>;
}

impl<T: ProtocolSim + ?Sized> MeteredProtocolSim for T {
    fn get_amount_out_metered<'a>(
        &self,
        component_id: &'a ComponentId,
        caller: SwapCaller,
        meter: &mut SolveMeter<'a>,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let started = Instant::now();
        let outcome = self.get_amount_out_guarded(amount_in, token_in, token_out);
        meter.record_call(component_id, caller, started.elapsed(), outcome.is_err());
        outcome
    }
}
