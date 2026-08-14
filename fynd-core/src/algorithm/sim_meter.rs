//! Measurement of the simulation work one solve does.
//!
//! Simulating a swap is the dominant cost of solving, and that cost varies by orders of magnitude
//! between protocols — a constant-product pool is arithmetic, a `vm:*` pool runs EVM bytecode. This
//! records what was asked of which component, so a solve can say where its time went.
//!
//! Callers go through [`MeteredProtocolSim::get_amount_out_metered`], which wraps the panic guard
//! in [`super::sim_guard`] rather than replacing it. Only `water_fill` uses it; every other pass
//! still takes the bare guard, so nothing outside a solve is counted.

#[cfg(not(feature = "swap-metrics"))]
use std::marker::PhantomData;
#[cfg(feature = "swap-metrics")]
use std::{
    cmp::Reverse,
    time::{Duration, Instant},
};

#[cfg(feature = "swap-metrics")]
use metrics::counter;
use num_bigint::BigUint;
#[cfg(feature = "swap-metrics")]
use rustc_hash::{FxHashMap, FxHashSet};
#[cfg(feature = "swap-metrics")]
use tracing::{debug, enabled, Level};
use tycho_simulation::tycho_common::{
    models::token::Token,
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
};

use super::{sim_guard::GuardedProtocolSim, water_fill::SolvePass};
use crate::{feed::market_data::MarketState, types::ComponentId};

/// The word the report prints for a pass.
#[cfg(feature = "swap-metrics")]
fn pass_name(pass: SolvePass) -> &'static str {
    match pass {
        SolvePass::Discovery => "discovery",
        SolvePass::Ranking => "ranking",
        SolvePass::SetSelection => "set-selection",
        SolvePass::Exchange => "exchange",
        SolvePass::Chunking => "chunking",
        SolvePass::Assembly => "assembly",
    }
}

#[cfg(feature = "swap-metrics")]
/// The swaps one component was asked for over a solve. Only some of them reached it — the rest
/// were answered from an amount it had already been asked, so they carry no call and no time.
#[derive(Default, Clone, Copy)]
struct ComponentSwaps {
    /// Calls made against this component.
    calls: u64,
    /// Of those, the ones that came back an error: the pool could not quote the swap, or its math
    /// panicked and [`GuardedProtocolSim`] turned that into an error.
    failed: u64,
    /// Swaps the cache answered instead, so no call was made at all.
    cache_hits: u64,
    /// Swaps answered by reading across two nearby amounts the pool had already been asked, so
    /// again no call was made. Only the passes that settle which paths get split do this, and the
    /// amount they get back is slightly below what the pool would have paid.
    interpolated: u64,
    /// Swaps refused without calling, because the pool had already refused a smaller amount.
    refused_without_calling: u64,
    /// Time spent inside the calls that were made.
    call_time: Duration,
}

#[cfg(feature = "swap-metrics")]
impl ComponentSwaps {
    fn add(&mut self, other: &ComponentSwaps) {
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
    #[cfg(feature = "swap-metrics")]
    by_component_and_pass: FxHashMap<(&'a ComponentId, SolvePass), ComponentSwaps>,
    #[cfg(not(feature = "swap-metrics"))]
    borrowed_ids: PhantomData<&'a ComponentId>,
}

impl<'a> SolveMeter<'a> {
    #[cfg(feature = "swap-metrics")]
    pub(crate) fn new() -> Self {
        Self { by_component_and_pass: FxHashMap::default() }
    }

    #[cfg(not(feature = "swap-metrics"))]
    pub(crate) fn new() -> Self {
        Self { borrowed_ids: PhantomData }
    }

    /// Records one `get_amount_out` call and how long it took.
    #[cfg(feature = "swap-metrics")]
    fn record_call(
        &mut self,
        component_id: &'a ComponentId,
        pass: SolvePass,
        call_time: Duration,
        failed: bool,
    ) {
        let counts = self.counts(component_id, pass);
        counts.calls += 1;
        counts.call_time += call_time;
        if failed {
            counts.failed += 1;
        }
    }

    /// Records a swap the cache answered, so no call was made.
    #[cfg(feature = "swap-metrics")]
    pub(crate) fn record_cache_hit(&mut self, component_id: &'a ComponentId, pass: SolvePass) {
        self.counts(component_id, pass)
            .cache_hits += 1;
    }

    /// Records a swap answered by reading across two nearby amounts, so no call was made.
    #[cfg(feature = "swap-metrics")]
    pub(crate) fn record_interpolation(&mut self, component_id: &'a ComponentId, pass: SolvePass) {
        self.counts(component_id, pass)
            .interpolated += 1;
    }

    /// Records a swap refused on the strength of a smaller amount the pool already refused.
    #[cfg(feature = "swap-metrics")]
    pub(crate) fn record_refusal_without_calling(
        &mut self,
        component_id: &'a ComponentId,
        pass: SolvePass,
    ) {
        self.counts(component_id, pass)
            .refused_without_calling += 1;
    }

    #[cfg(feature = "swap-metrics")]
    fn counts(&mut self, component_id: &'a ComponentId, pass: SolvePass) -> &mut ComponentSwaps {
        self.by_component_and_pass
            .entry((component_id, pass))
            .or_default()
    }

    /// Writes what the solve asked of which protocol and which pass, and feeds the same counts to
    /// the metrics recorder. A component the market no longer holds is reported under `unknown`
    /// rather than dropped, so the totals still add up.
    ///
    /// The counters always go out. The two lines are only built when debug logging is on, since
    /// formatting them walks every protocol and every pass on a path that runs once per solve.
    #[cfg(feature = "swap-metrics")]
    pub(crate) fn report(&self, market: &MarketState, solve_time_ms: u64) {
        let mut by_protocol: FxHashMap<&str, ComponentSwaps> = FxHashMap::default();
        let mut by_pass: FxHashMap<SolvePass, ComponentSwaps> = FxHashMap::default();
        let mut components: FxHashSet<&ComponentId> = FxHashSet::default();
        let mut totals = ComponentSwaps::default();
        for ((component_id, pass), counts) in &self.by_component_and_pass {
            let protocol = market
                .get_component(component_id)
                .map_or("unknown", |component| component.protocol_system.as_str());
            by_protocol
                .entry(protocol)
                .or_default()
                .add(counts);
            by_pass
                .entry(*pass)
                .or_default()
                .add(counts);
            components.insert(component_id);
            totals.add(counts);
        }

        let mut costliest_first: Vec<(&str, ComponentSwaps)> = by_protocol.into_iter().collect();
        costliest_first
            .sort_unstable_by_key(|(protocol, counts)| (Reverse(counts.call_time), *protocol));

        for (protocol, counts) in &costliest_first {
            counter!("water_fill.get_amount_out_calls", "protocol" => protocol.to_string())
                .increment(counts.calls);
            counter!("water_fill.failed_calls", "protocol" => protocol.to_string())
                .increment(counts.failed);
            counter!("water_fill.cache_hits", "protocol" => protocol.to_string())
                .increment(counts.cache_hits);
            counter!("water_fill.interpolated_swaps", "protocol" => protocol.to_string())
                .increment(counts.interpolated);
            counter!("water_fill.refused_without_calling", "protocol" => protocol.to_string())
                .increment(counts.refused_without_calling);
        }

        if !enabled!(Level::DEBUG) {
            return;
        }

        let mut passes: Vec<(SolvePass, ComponentSwaps)> = by_pass.into_iter().collect();
        passes.sort_unstable_by_key(|(_, counts)| Reverse(counts.call_time));
        let per_pass = passes
            .iter()
            .map(|(pass, counts)| {
                format!(
                    "{}: {} calls in {:.1}ms, {} answered without calling",
                    pass_name(*pass),
                    counts.calls,
                    counts.call_time.as_secs_f64() * 1000.0,
                    counts.cache_hits + counts.interpolated + counts.refused_without_calling,
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        debug!(solve_time_ms, "water-fill simulation by pass: {per_pass}");

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
    /// Recording is compiled out. The arguments are taken so the call sites read the same in
    /// either build; the optimiser drops them.
    #[cfg(not(feature = "swap-metrics"))]
    pub(crate) fn record_cache_hit(&mut self, _component_id: &'a ComponentId, _pass: SolvePass) {}

    /// Recording is compiled out. See [`SolveMeter::record_cache_hit`].
    #[cfg(not(feature = "swap-metrics"))]
    pub(crate) fn record_interpolation(
        &mut self,
        _component_id: &'a ComponentId,
        _pass: SolvePass,
    ) {
    }

    /// Recording is compiled out. See [`SolveMeter::record_cache_hit`].
    #[cfg(not(feature = "swap-metrics"))]
    pub(crate) fn record_refusal_without_calling(
        &mut self,
        _component_id: &'a ComponentId,
        _pass: SolvePass,
    ) {
    }

    /// There is nothing to report without `swap-metrics`.
    #[cfg(not(feature = "swap-metrics"))]
    pub(crate) fn report(&self, _market: &MarketState, _solve_time_ms: u64) {}
}

/// Extension trait adding metered, panic-guarded simulation calls to every [`ProtocolSim`].
///
/// Wraps [`GuardedProtocolSim::get_amount_out_guarded`] and books the call against the component
/// that served it, which the guard alone cannot do — it sees only the state and the two tokens.
pub(crate) trait MeteredProtocolSim {
    /// Calls the panic-guarded `get_amount_out` and records it against `component_id` and `pass`.
    fn get_amount_out_metered<'a>(
        &self,
        component_id: &'a ComponentId,
        pass: SolvePass,
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
        pass: SolvePass,
        meter: &mut SolveMeter<'a>,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        #[cfg(not(feature = "swap-metrics"))]
        {
            let _ = (component_id, pass, meter);
            self.get_amount_out_guarded(amount_in, token_in, token_out)
        }
        #[cfg(feature = "swap-metrics")]
        {
            let started = Instant::now();
            let outcome = self.get_amount_out_guarded(amount_in, token_in, token_out);
            meter.record_call(component_id, pass, started.elapsed(), outcome.is_err());
            outcome
        }
    }
}
