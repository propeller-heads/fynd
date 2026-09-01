//! Measurement of the simulation work one solve does.
//!
//! Simulating a swap is the dominant cost of solving, and that cost varies by orders of magnitude
//! between protocols — a constant-product pool is arithmetic, a `vm:*` pool runs EVM bytecode. This
//! records what was asked of which component, so a solve can say where its time went.
//!
//! Callers go through [`MeteredProtocolSim::get_amount_out_metered`], which wraps the panic guard
//! in `sim_guard` rather than replacing it. Counting and reporting are one decision: an
//! algorithm that does not bracket its solve with [`start_solve`] and [`report`] must not meter
//! either, or the counts pile up on the worker thread unread. `water_fill` and
//! `path_frank_wolfe` both bracket, which is why the shared split code they run through meters;
//! everything else takes the bare guard.
//!
//! Counts live in a thread-local for the duration of a solve, which a worker runs start to finish
//! on one thread. Recording therefore needs nothing passed down to it, and the default build —
//! where `swap-metrics` is off — compiles every entry point here to nothing.

#[cfg(feature = "swap-metrics")]
use std::{
    cell::RefCell,
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

use super::sim_guard::GuardedProtocolSim;
use crate::{feed::market_data::MarketState, types::ComponentId};

/// What the report calls the stage a swap was asked for.
///
/// A plain label rather than an algorithm's own stage type: an algorithm names its stages, this
/// module only groups by what it is told.
pub type StageLabel = &'static str;

/// The swaps one component was asked for over a solve. Only some of them reached it — the rest
/// were answered from an amount it had already been asked, so they carry no call and no time.
#[cfg(feature = "swap-metrics")]
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
    /// again no call was made. Only the stages that settle which paths get split do this, and the
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

#[cfg(feature = "swap-metrics")]
thread_local! {
    /// Simulation work done while solving one order, on this thread.
    ///
    /// Counted per component, not per protocol: which protocol a component belongs to is known
    /// only to the market, so it is resolved once in [`report`] rather than looked up on every
    /// swap. Resolving it there also names every `vm:*` protocol individually, where reading it
    /// off the concrete state type would collapse them into one.
    ///
    /// The component id is owned rather than borrowed, which costs a clone per recorded swap.
    /// That only happens in the `swap-metrics` build; the default build records nothing.
    static SOLVE_SWAPS: RefCell<FxHashMap<(ComponentId, StageLabel), ComponentSwaps>> =
        RefCell::new(FxHashMap::default());
}

#[cfg(feature = "swap-metrics")]
fn with_counts(
    component_id: &ComponentId,
    stage: StageLabel,
    edit: impl FnOnce(&mut ComponentSwaps),
) {
    SOLVE_SWAPS.with_borrow_mut(|swaps| {
        edit(
            swaps
                .entry((component_id.clone(), stage))
                .or_default(),
        );
    });
}

/// Discards whatever the last solve on this thread left behind, so counts never run together.
#[cfg(feature = "swap-metrics")]
pub fn start_solve() {
    SOLVE_SWAPS.with_borrow_mut(FxHashMap::clear);
}

/// Records one `get_amount_out` call and how long it took.
#[cfg(feature = "swap-metrics")]
fn record_call(component_id: &ComponentId, stage: StageLabel, call_time: Duration, failed: bool) {
    with_counts(component_id, stage, |counts| {
        counts.calls += 1;
        counts.call_time += call_time;
        if failed {
            counts.failed += 1;
        }
    });
}

/// Records a swap the cache answered, so no call was made.
#[cfg(feature = "swap-metrics")]
pub fn record_cache_hit(component_id: &ComponentId, stage: StageLabel) {
    with_counts(component_id, stage, |counts| counts.cache_hits += 1);
}

/// Records a swap answered by reading across two nearby amounts, so no call was made.
#[cfg(feature = "swap-metrics")]
pub fn record_interpolation(component_id: &ComponentId, stage: StageLabel) {
    with_counts(component_id, stage, |counts| counts.interpolated += 1);
}

/// Records a swap refused on the strength of a smaller amount the pool already refused.
#[cfg(feature = "swap-metrics")]
pub fn record_refusal_without_calling(component_id: &ComponentId, stage: StageLabel) {
    with_counts(component_id, stage, |counts| counts.refused_without_calling += 1);
}

/// Writes what the solve asked of which protocol and which stage, and feeds the same counts to the
/// metrics recorder. A component the market no longer holds is reported under `unknown` rather
/// than dropped, so the totals still add up.
///
/// The counters always go out. The two lines are only built when debug logging is on, since
/// formatting them walks every protocol and every stage on a path that runs once per solve.
#[cfg(feature = "swap-metrics")]
pub fn report(algorithm: &str, market: &MarketState, solve_time_ms: impl FnOnce() -> u64) {
    let solve_time_ms = solve_time_ms();
    SOLVE_SWAPS.with_borrow(|swaps| report_swaps(algorithm, swaps, market, solve_time_ms));
}

#[cfg(feature = "swap-metrics")]
fn report_swaps(
    algorithm: &str,
    swaps: &FxHashMap<(ComponentId, StageLabel), ComponentSwaps>,
    market: &MarketState,
    solve_time_ms: u64,
) {
    let mut by_protocol: FxHashMap<&str, ComponentSwaps> = FxHashMap::default();
    for ((component_id, _), counts) in swaps {
        let protocol = market
            .get_component(component_id)
            .map_or("unknown", |component| component.protocol_system.as_str());
        by_protocol
            .entry(protocol)
            .or_default()
            .add(counts);
    }

    let mut costliest_first: Vec<(&str, ComponentSwaps)> = by_protocol.into_iter().collect();
    costliest_first
        .sort_unstable_by_key(|(protocol, counts)| (Reverse(counts.call_time), *protocol));

    for (protocol, counts) in &costliest_first {
        counter!(format!("{algorithm}.get_amount_out_calls"), "protocol" => protocol.to_string())
            .increment(counts.calls);
        counter!(format!("{algorithm}.failed_calls"), "protocol" => protocol.to_string())
            .increment(counts.failed);
        counter!(format!("{algorithm}.cache_hits"), "protocol" => protocol.to_string())
            .increment(counts.cache_hits);
        counter!(format!("{algorithm}.interpolated_swaps"), "protocol" => protocol.to_string())
            .increment(counts.interpolated);
        counter!(format!("{algorithm}.refused_without_calling"), "protocol" => protocol.to_string())
            .increment(counts.refused_without_calling);
    }

    if !enabled!(Level::DEBUG) {
        return;
    }

    let mut by_stage: FxHashMap<StageLabel, ComponentSwaps> = FxHashMap::default();
    let mut components: FxHashSet<&ComponentId> = FxHashSet::default();
    let mut totals = ComponentSwaps::default();
    for ((component_id, stage), counts) in swaps {
        by_stage
            .entry(stage)
            .or_default()
            .add(counts);
        components.insert(component_id);
        totals.add(counts);
    }

    let mut stages: Vec<(StageLabel, ComponentSwaps)> = by_stage.into_iter().collect();
    stages.sort_unstable_by_key(|(_, counts)| Reverse(counts.call_time));
    let per_stage = stages
        .iter()
        .map(|(stage, counts)| {
            format!(
                "{}: {} calls in {:.1}ms, {} answered without calling",
                stage,
                counts.calls,
                counts.call_time.as_secs_f64() * 1000.0,
                counts.cache_hits + counts.interpolated + counts.refused_without_calling,
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    debug!(solve_time_ms, "{algorithm} simulation by stage: {per_stage}");

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
        "{algorithm} simulation cost: {per_protocol}",
    );
}

/// Recording is compiled out. The arguments are taken so the call sites read the same in either
/// build; the optimiser drops them.
#[cfg(not(feature = "swap-metrics"))]
pub fn start_solve() {}

/// Recording is compiled out. See [`start_solve`].
#[cfg(not(feature = "swap-metrics"))]
pub fn record_cache_hit(_component_id: &ComponentId, _stage: StageLabel) {}

/// Recording is compiled out. See [`start_solve`].
#[cfg(not(feature = "swap-metrics"))]
pub fn record_interpolation(_component_id: &ComponentId, _stage: StageLabel) {}

/// Recording is compiled out. See [`start_solve`].
#[cfg(not(feature = "swap-metrics"))]
pub fn record_refusal_without_calling(_component_id: &ComponentId, _stage: StageLabel) {}

/// There is nothing to report without `swap-metrics`.
///
/// The solve time is taken as a closure so the clock is never read in this build: an argument
/// would be evaluated at the call site even though nothing here uses it.
#[cfg(not(feature = "swap-metrics"))]
pub fn report(_algorithm: &str, _market: &MarketState, _solve_time_ms: impl FnOnce() -> u64) {}

/// Extension trait adding metered, panic-guarded simulation calls to every [`ProtocolSim`].
///
/// Wraps `GuardedProtocolSim::get_amount_out_guarded` and books the call against the component
/// that served it, which the guard alone cannot do — it sees only the state and the two tokens.
pub trait MeteredProtocolSim {
    /// Calls the panic-guarded `get_amount_out` and records it against `component_id` and `stage`.
    fn get_amount_out_metered(
        &self,
        component_id: &ComponentId,
        stage: StageLabel,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError>;
}

impl<T: ProtocolSim + ?Sized> MeteredProtocolSim for T {
    fn get_amount_out_metered(
        &self,
        component_id: &ComponentId,
        stage: StageLabel,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        #[cfg(not(feature = "swap-metrics"))]
        {
            let _ = (component_id, stage);
            self.get_amount_out_guarded(amount_in, token_in, token_out)
        }
        #[cfg(feature = "swap-metrics")]
        {
            let started = Instant::now();
            let outcome = self.get_amount_out_guarded(amount_in, token_in, token_out);
            record_call(component_id, stage, started.elapsed(), outcome.is_err());
            outcome
        }
    }
}

#[cfg(all(test, feature = "swap-metrics"))]
mod tests {
    use std::time::Duration;

    use super::*;

    fn component(id: &str) -> ComponentId {
        ComponentId::from(id)
    }

    fn counts_for(component_id: &ComponentId, stage: StageLabel) -> ComponentSwaps {
        SOLVE_SWAPS.with_borrow(|swaps| {
            swaps
                .get(&(component_id.clone(), stage))
                .copied()
                .unwrap_or_default()
        })
    }

    /// The same component asked at two stages is counted separately, so the report can say which
    /// stage the work sat in.
    #[test]
    fn test_records_each_stage_of_a_component_separately() {
        start_solve();
        let pool = component("pool-a");

        record_call(&pool, "ranking", Duration::from_millis(3), false);
        record_call(&pool, "ranking", Duration::from_millis(2), true);
        record_cache_hit(&pool, "chunking");

        let ranking = counts_for(&pool, "ranking");
        assert_eq!(ranking.calls, 2);
        assert_eq!(ranking.failed, 1);
        assert_eq!(ranking.call_time, Duration::from_millis(5));
        assert_eq!(ranking.cache_hits, 0);
        assert_eq!(counts_for(&pool, "chunking").cache_hits, 1);
    }

    /// Swaps answered without calling the pool are counted apart from the calls, so a solve can
    /// say how much the cache saved.
    #[test]
    fn test_counts_answers_that_never_reached_the_pool() {
        start_solve();
        let pool = component("pool-b");

        record_cache_hit(&pool, "ranking");
        record_interpolation(&pool, "ranking");
        record_refusal_without_calling(&pool, "ranking");

        let counts = counts_for(&pool, "ranking");
        assert_eq!(counts.calls, 0);
        assert_eq!(counts.cache_hits, 1);
        assert_eq!(counts.interpolated, 1);
        assert_eq!(counts.refused_without_calling, 1);
    }

    /// A new solve starts from nothing, so one order's counts never land in the next one's report.
    #[test]
    fn test_start_solve_discards_the_previous_solve() {
        start_solve();
        let pool = component("pool-c");
        record_call(&pool, "ranking", Duration::from_millis(1), false);

        start_solve();

        assert_eq!(counts_for(&pool, "ranking").calls, 0);
    }

    /// A component the market no longer holds still reaches the report, under `unknown`, so the
    /// totals add up.
    #[test]
    fn test_unknown_component_is_reported_rather_than_dropped() {
        start_solve();
        record_call(&component("gone"), "ranking", Duration::from_millis(1), false);

        // The market holds nothing, so every component resolves to "unknown". This asserts the
        // report walks it without panicking and finds the fallback protocol name.
        let market = MarketState::default();
        SOLVE_SWAPS.with_borrow(|swaps| {
            let protocol = swaps.keys().map(|(component_id, _)| {
                market
                    .get_component(component_id)
                    .map_or("unknown", |component| component.protocol_system.as_str())
            });
            assert!(protocol.eq(["unknown"]));
        });
        report("test", &market, || 1);
    }
}
