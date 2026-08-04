//! Route decay measured in non-overlapping rounds: quote a sample of trades at one block, then
//! replay those exact routes at each of the following `--offsets` blocks.
//!
//! Each offset yields three numbers. The quoted route replayed at the later state gives the **total
//! move**; a freshly solved quote at that same state gives the part of it that was **unavoidable
//! market drift**; the difference is what holding a **stale route** cost. Only the last is
//! something better routing or faster submission can recover.
//!
//! The trades are synthetic on purpose. `monitor`'s existing `slippage` field solves a settled
//! trade at N-1 and replays it at N — a state that already contains that trade's own price impact,
//! so it measures the route eating its own shadow. Here the sampled shapes never execute: they are
//! drawn from historical flow (see [`sample`]) and re-quoted at unrelated live blocks, so nothing
//! but genuine market movement and route staleness can move the measurement.
//!
//! Rounds do not overlap — a new sample is drawn only once the previous round's last offset is
//! measured — so per-block work is `sample_size` replays plus `sample_size` solves regardless of
//! how many offsets are configured.

pub(crate) mod record;
pub(crate) mod sample;
mod summary;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use tracing::{info, warn};

use crate::{
    decay::{
        record::{DecayRecord, Measurement, QuotedTrade, ReplayFailure},
        sample::{Sampler, TradeShape},
        summary::Summary,
    },
    resolve::{
        jsonl::RotatingWriter,
        session::{self, Session, SolverArgs},
        step::{Encoding, StepAdapter},
        Outcome, SteppingSolver,
    },
    telemetry,
};

/// Filename prefix for decay output. Deliberately not `comparisons`, which [`sample`] globs as its
/// input — sharing it would let a run read its own output back as trade shapes.
const DECAY_PREFIX: &str = "decay";

/// Fraction of a block's time the per-block measurement may occupy before it is warned about. Above
/// this there is no margin left for feed jitter, and the run is one slow solve away from sliding
/// behind head.
const PACING_WARN_FRACTION: f64 = 0.5;

/// Inputs for the decay measurement — the `decay` subcommand's CLI arguments.
#[derive(clap::Args)]
pub(crate) struct DecayArgs {
    #[command(flatten)]
    pub solver: SolverArgs,

    /// Directory of a `monitor` run's `comparisons-*.jsonl` to draw trade shapes from. Only each
    /// record's token pair and input amount are read; nothing about how it settled is used
    #[arg(long)]
    pub comparisons_dir: PathBuf,

    /// Venue whose flow to sample, matched case-insensitively against each record's `venue`
    #[arg(long, default_value = sample::RELAY_VENUE)]
    pub venue: String,

    /// Trades quoted per round. Every one is re-solved at every offset, so this sets the per-block
    /// solver load: budget roughly 30ms per trade and keep the total well inside one block (on
    /// Base's 2s blocks, 25 trades ≈ 750ms worst case)
    #[arg(long, default_value_t = 25)]
    pub sample_size: usize,

    /// How many blocks after the quote to measure. Decay is front-loaded, so the first few offsets
    /// carry most of the signal
    #[arg(long, default_value_t = 5)]
    pub offsets: u32,

    /// Seed for the trade draw, so a run's sample is reproducible
    #[arg(long, default_value_t = 0)]
    pub seed: u64,

    /// Write one JSON line per (trade, offset) into this directory as `decay-YYYY-MM-DD.jsonl`,
    /// rotated at each UTC day boundary
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
}

/// One session's terminal condition: the run completed (`--max-blocks` reached) or the session is
/// unhealthy — the feed died — and the caller should rebuild the solver.
enum SessionEnd {
    Complete,
    Unhealthy(String),
}

/// Counters that survive feed rebuilds, so `--max-blocks` and the final tally span the whole run.
#[derive(Default)]
struct Totals {
    /// Blocks whose measurement work completed, the unit `--max-blocks` counts.
    blocks: u64,
    rounds: u64,
    /// Rounds abandoned because the feed died before their last offset.
    abandoned_rounds: u64,
    records: u64,
    /// Sampled shapes the solver could not quote at all, so they never entered a round.
    unquotable: u64,
    pool_gone: u64,
    simulation_failed: u64,
    no_market_reference: u64,
}

impl Totals {
    fn count_failure(&mut self, failure: ReplayFailure) {
        match failure {
            ReplayFailure::PoolGone => self.pool_gone += 1,
            ReplayFailure::SimulationFailed => self.simulation_failed += 1,
            ReplayFailure::NoMarketReference => self.no_market_reference += 1,
        }
    }
}

/// Build the in-process stepped solver and measure route decay in rounds until `--max-blocks` or
/// Ctrl-C. When the tycho feed dies the solver is torn down and rebuilt in place, and the round in
/// flight is abandoned rather than finished against a different solver's states.
pub(crate) async fn run(cfg: DecayArgs) -> anyhow::Result<()> {
    anyhow::ensure!(cfg.offsets >= 1, "--offsets must be at least 1");
    anyhow::ensure!(cfg.sample_size >= 1, "--sample-size must be at least 1");

    let shapes = sample::load_shapes(&cfg.comparisons_dir, &cfg.venue)?;
    // Taken before the solver args are moved into the session, which owns them from here on.
    let rounds = RoundConfig {
        sample_size: cfg.sample_size,
        offsets: cfg.offsets,
        max_blocks: cfg.solver.max_blocks,
    };
    let session = Session::prepare(cfg.solver).await?;
    let block_budget = session::block_time(session.chain()).mul_f64(PACING_WARN_FRACTION);
    info!(
        sample_size = cfg.sample_size,
        offsets = cfg.offsets,
        pool = shapes.len(),
        seed = cfg.seed,
        block_budget_ms = block_budget.as_millis(),
        "measuring route decay in rounds"
    );

    if let Some(port) = session.args().metrics_port {
        telemetry::install_exporter(port)?;
        info!(port, "serving Prometheus metrics at /metrics");
    }

    let output = match cfg.output_dir.as_ref() {
        Some(dir) => {
            let writer = RotatingWriter::open(dir, DECAY_PREFIX)?;
            info!(path = %writer.current_path().display(), "appending decay records to JSONL");
            Some(writer)
        }
        None => None,
    };

    let mut state = RunState {
        sampler: Sampler::new(cfg.seed, shapes.len()),
        summary: Summary::new(cfg.offsets),
        totals: Totals::default(),
        output,
        block_budget,
    };

    // Resolves on Ctrl-C, so a long run stops cleanly at any await below — including the
    // multi-minute solver builds. On shutdown the round in flight is abandoned and the output
    // writer flushes as it drops.
    let shutdown = session::shutdown_signal();
    tokio::pin!(shutdown);

    // The first build fails fast — an error here is a configuration problem. Rebuilds after a feed
    // death retry forever, since the config is known good and failures are transient.
    let (mut solver, mut controller) = tokio::select! {
        biased;
        () = &mut shutdown => return Ok(()),
        built = session.build() => built?,
    };
    loop {
        // Encoding off: decay reads only `amount_out`, so calldata is work we would throw away —
        // and an encode failure would drop a route that measures perfectly well.
        let adapter =
            StepAdapter::new(&solver, &controller, session.args().timeout_ms, Encoding::Skipped);
        let reason = tokio::select! {
            biased;
            () = &mut shutdown => {
                info!("received Ctrl-C; shutting down");
                break;
            }
            end = run_session(&adapter, &rounds, &shapes, &mut state) => match end {
                SessionEnd::Complete => break,
                SessionEnd::Unhealthy(reason) => reason,
            },
        };
        warn!(reason, "session unhealthy; rebuilding the solver to resubscribe");
        telemetry::record_feed_rebuild();
        solver.shutdown();
        let Some(built) = session.rebuild(shutdown.as_mut()).await else {
            info!("received Ctrl-C during rebuild; shutting down");
            report(&state);
            return Ok(());
        };
        (solver, controller) = built;
    }
    solver.shutdown();
    report(&state);
    Ok(())
}

/// The round-shaping knobs, lifted out of [`DecayArgs`] before its solver args are moved into the
/// [`Session`] that owns them.
struct RoundConfig {
    sample_size: usize,
    offsets: u32,
    max_blocks: Option<u64>,
}

/// Run state that outlives any one solver session, so a feed rebuild loses only the round in
/// flight.
struct RunState {
    sampler: Sampler,
    summary: Summary,
    totals: Totals,
    output: Option<RotatingWriter>,
    block_budget: Duration,
}

/// Drive one solver session: draw a sample, quote it, measure it across the configured offsets, and
/// repeat until the run completes or the feed dies.
async fn run_session<S: SteppingSolver + ?Sized>(
    adapter: &S,
    cfg: &RoundConfig,
    shapes: &[TradeShape],
    state: &mut RunState,
) -> SessionEnd {
    // A quote needs an applied state to be quoted against.
    if adapter.current_block().await.is_none() {
        info!("waiting for solver to apply its first block…");
        if let Err(e) = adapter.advance().await {
            return SessionEnd::Unhealthy(e.to_string());
        }
    }

    loop {
        let Some(quote_block) = adapter.current_block().await else {
            warn!("no applied block yet; advancing");
            if let Err(e) = adapter.advance().await {
                return SessionEnd::Unhealthy(e.to_string());
            }
            continue;
        };

        let quoted = quote_round(adapter, cfg, shapes, state, quote_block).await;
        state.totals.rounds += 1;
        let round_id = state.totals.rounds;
        if quoted.is_empty() {
            // Nothing to replay, but the offsets must still elapse — otherwise the next round would
            // quote against the very state this one did.
            warn!(
                block = quote_block,
                round_id, "no sampled trade could be quoted; skipping the round"
            );
            for _ in 0..cfg.offsets {
                if let Err(e) = adapter.advance().await {
                    return SessionEnd::Unhealthy(e.to_string());
                }
            }
            continue;
        }

        for offset in 1..=cfg.offsets {
            if let Err(e) = adapter.advance().await {
                // The round dies with the session: its remaining offsets would be measured against
                // a rebuilt solver's states, which are not continuous with this
                // round's quote.
                state.totals.abandoned_rounds += 1;
                return SessionEnd::Unhealthy(e.to_string());
            }
            let started = Instant::now();
            let Some(measured_block) = adapter.current_block().await else {
                warn!(round_id, offset, "no applied block after advancing; abandoning the round");
                state.totals.abandoned_rounds += 1;
                break;
            };
            let at = RoundAt { round_id, quote_block, measured_block, offset };
            measure_offset(adapter, state, &quoted, at).await;

            let elapsed = started.elapsed();
            telemetry::record_block_seconds(elapsed.as_secs_f64());
            if elapsed > state.block_budget {
                warn!(
                    round_id,
                    offset,
                    elapsed_ms = elapsed.as_millis(),
                    budget_ms = state.block_budget.as_millis(),
                    trades = quoted.len(),
                    "measurement exceeded its per-block budget; lower --sample-size"
                );
            }

            state.totals.blocks += 1;
            info!(
                block = measured_block,
                round_id,
                offset,
                trades = quoted.len(),
                elapsed_s = elapsed.as_secs_f64(),
                "measured offset"
            );
            if cfg
                .max_blocks
                .is_some_and(|max| state.totals.blocks >= max)
            {
                info!(blocks = state.totals.blocks, "reached --max-blocks");
                return SessionEnd::Complete;
            }
        }
    }
}

/// Identifies one offset measurement within a round.
struct RoundAt {
    round_id: u64,
    quote_block: u64,
    measured_block: u64,
    offset: u32,
}

/// Draw a fresh sample and quote each of its trades at the current state.
///
/// Only quotes carrying a route are kept: [`SteppingSolver::reexecute`] has nothing to replay
/// without one, so such a trade could never be measured and is counted as unquotable instead.
async fn quote_round<S: SteppingSolver + ?Sized>(
    adapter: &S,
    cfg: &RoundConfig,
    shapes: &[TradeShape],
    state: &mut RunState,
    quote_block: u64,
) -> Vec<QuotedTrade> {
    let started = Instant::now();
    let mut quoted = Vec::with_capacity(cfg.sample_size);
    for index in state.sampler.draw(cfg.sample_size) {
        let Some(shape) = shapes.get(index) else {
            continue;
        };
        match adapter
            .solve(shape.token_in, shape.token_out, shape.amount_in)
            .await
        {
            Outcome::Solved(quote) if quote.solved_route.is_some() => {
                quoted.push(QuotedTrade { shape: shape.clone(), quote });
            }
            Outcome::Solved(_) | Outcome::Partial(_) | Outcome::Unsolvable(_) => {
                state.totals.unquotable += 1;
            }
        }
    }
    info!(
        block = quote_block,
        quoted = quoted.len(),
        requested = cfg.sample_size,
        elapsed_s = started.elapsed().as_secs_f64(),
        "quoted a fresh sample"
    );
    quoted
}

/// Measure every quoted trade at the solver's current state: replay its route, solve it fresh for
/// the market-movement reference, and emit one record per trade.
async fn measure_offset<S: SteppingSolver + ?Sized>(
    adapter: &S,
    state: &mut RunState,
    quoted: &[QuotedTrade],
    at: RoundAt,
) {
    let mut records = Vec::with_capacity(quoted.len());
    for trade in quoted {
        let replayed = match adapter.reexecute(&trade.quote).await {
            Outcome::Solved(replay) => Ok(replay.amount_out),
            Outcome::Partial(reason) | Outcome::Unsolvable(reason) => {
                Err(ReplayFailure::classify(&reason))
            }
        };
        // The fresh solve is the market-movement reference: what any router would have got at this
        // state, so whatever the replay lost beyond it is down to the route being stale.
        let fresh = match adapter
            .solve(trade.shape.token_in, trade.shape.token_out, trade.shape.amount_in)
            .await
        {
            Outcome::Solved(quote) => Some(quote.amount_out),
            Outcome::Partial(_) | Outcome::Unsolvable(_) => None,
        };
        let record = DecayRecord::build(
            trade,
            Measurement {
                round_id: at.round_id,
                offset: at.offset,
                quote_block: at.quote_block,
                measured_block: at.measured_block,
                replayed,
                fresh,
            },
        );
        if let Some(failure) = record.failure {
            state.totals.count_failure(failure);
        }
        if let Some(bps) = record.bps {
            state.summary.record(at.offset, bps);
        }
        records.push(record);
    }
    state.totals.records += records.len() as u64;
    if let Some(writer) = state.output.as_mut() {
        write_records(writer, &records);
    }
}

/// Append one JSON line per record. A write failure is logged and abandons the batch rather than
/// taking the run down: the measurement is still valid, and a long run should not die on a full
/// disk.
fn write_records(rotating: &mut RotatingWriter, records: &[DecayRecord]) {
    use std::io::Write;

    let writer = rotating.writer();
    for record in records {
        let Ok(line) = serde_json::to_string(record) else {
            continue;
        };
        if let Err(e) = writeln!(writer, "{line}") {
            warn!(error = %e, "failed to write decay record");
            return;
        }
    }
    if let Err(e) = writer.flush() {
        warn!(error = %e, "failed to flush decay writer");
    }
}

/// Log the run's per-offset statistics and coverage tally.
fn report(state: &RunState) {
    let totals = &state.totals;
    info!(
        rounds = totals.rounds,
        blocks = totals.blocks,
        records = totals.records,
        abandoned_rounds = totals.abandoned_rounds,
        unquotable_samples = totals.unquotable,
        pool_gone = totals.pool_gone,
        simulation_failed = totals.simulation_failed,
        no_market_reference = totals.no_market_reference,
        "decay run finished"
    );
    let stats = state.summary.stats();
    if stats.is_empty() {
        warn!("no complete measurements — nothing to summarize");
        return;
    }
    // Positive is surplus throughout, so degradation reads negative and the bad tail is p05/p01.
    // PR #297 reported the opposite sign; flip these before comparing against its numbers.
    for offset in &stats {
        info!(
            offset = offset.offset,
            count = offset.count,
            mean_bps = offset.mean_bps,
            winsorized_mean_bps = offset.winsorized_mean_bps,
            p50_bps = offset.p50_bps,
            p05_bps = offset.p05_bps,
            p01_bps = offset.p01_bps,
            degraded_share = offset.degraded_share,
            tail_share_beyond_20bps = offset.tail_share,
            market_share = offset.market_share,
            execution_share = offset.execution_share,
            "offset summary (positive bps = surplus)"
        );
    }
    match serde_json::to_string(&stats) {
        Ok(json) => info!(summary = %json, "decay summary json"),
        Err(e) => warn!(error = %e, "failed to serialize the decay summary"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    use alloy::primitives::{Address, U256};
    use async_trait::async_trait;

    use super::*;
    use crate::resolve::SolvedAmount;

    /// A stepping mock that walks a scripted list of per-block outputs.
    ///
    /// `advance()` moves to the next block. At each block, `solve` returns that block's `fresh`
    /// output and `reexecute` returns its `replayed` output, so a test can dictate the exact bps at
    /// every offset. `advance` fails once the script runs out, which stands in for a dead feed.
    struct ScriptedSolver {
        /// `(replayed, fresh)` per block, indexed by how many times `advance` has run.
        script: Vec<(Option<u64>, Option<u64>)>,
        block: AtomicUsize,
        /// Every `(token_in, amount_in)` handed to `solve`, so a test can see what was quoted.
        solved: Mutex<Vec<(Address, U256)>>,
    }

    impl ScriptedSolver {
        fn new(script: Vec<(Option<u64>, Option<u64>)>) -> Self {
            Self { script, block: AtomicUsize::new(0), solved: Mutex::new(Vec::new()) }
        }

        fn step(&self) -> usize {
            self.block.load(Ordering::Relaxed)
        }

        fn outcome(amount: Option<u64>, with_route: bool) -> Outcome {
            let Some(amount) = amount else {
                return Outcome::Unsolvable("scripted miss".to_string());
            };
            Outcome::Solved(SolvedAmount {
                amount_out: U256::from(amount),
                amount_out_net_gas: U256::from(amount),
                gas_estimate: U256::from(21_000),
                algorithm: "scripted".to_string(),
                quote_json: None,
                // A quote must carry a route to be replayable; a re-execution never does.
                solved_route: with_route.then(|| {
                    Box::new(crate::resolve::test_support::route(&[("uniswap_v2", "USDC", "WETH")]))
                }),
            })
        }
    }

    #[async_trait]
    impl SteppingSolver for ScriptedSolver {
        async fn current_block(&self) -> Option<u64> {
            Some(1_000 + u64::try_from(self.step()).unwrap_or(0))
        }

        async fn solve(&self, token_in: Address, _: Address, amount_in: U256) -> Outcome {
            self.solved
                .lock()
                .expect("lock")
                .push((token_in, amount_in));
            let fresh = self
                .script
                .get(self.step())
                .and_then(|(_, fresh)| *fresh);
            Self::outcome(fresh, true)
        }

        async fn advance(&self) -> anyhow::Result<()> {
            let next = self
                .block
                .fetch_add(1, Ordering::Relaxed) +
                1;
            anyhow::ensure!(next < self.script.len(), "scripted feed exhausted");
            Ok(())
        }

        async fn reexecute(&self, _: &SolvedAmount) -> Outcome {
            let replayed = self
                .script
                .get(self.step())
                .and_then(|(replayed, _)| *replayed);
            Self::outcome(replayed, false)
        }
    }

    fn shapes(count: usize) -> Vec<TradeShape> {
        (0..count)
            .map(|i| TradeShape {
                token_in: Address::repeat_byte(u8::try_from(i + 1).unwrap_or(0xff)),
                token_out: Address::repeat_byte(0xee),
                amount_in: U256::from(1_000 + i),
            })
            .collect()
    }

    fn state(offsets: u32, pool_len: usize) -> RunState {
        RunState {
            sampler: Sampler::new(1, pool_len),
            summary: Summary::new(offsets),
            totals: Totals::default(),
            output: None,
            block_budget: Duration::from_secs(1),
        }
    }

    fn config(sample_size: usize, offsets: u32, max_blocks: Option<u64>) -> RoundConfig {
        RoundConfig { sample_size, offsets, max_blocks }
    }

    #[tokio::test]
    async fn a_round_measures_every_offset_exactly_once() {
        // Block 0 quotes at 10_000; blocks 1..=5 each replay and re-solve.
        let mut script = vec![(None, Some(10_000))];
        script.extend((1..=5).map(|_| (Some(9_900), Some(9_950))));
        let solver = ScriptedSolver::new(script);
        let pool = shapes(1);
        let mut run = state(5, pool.len());

        // --max-blocks equals the offsets, so the run stops exactly at the round's end.
        let end = run_session(&solver, &config(1, 5, Some(5)), &pool, &mut run).await;
        assert!(matches!(end, SessionEnd::Complete));

        let stats = run.summary.stats();
        assert_eq!(
            stats
                .iter()
                .map(|s| s.offset)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5],
            "each offset must be measured"
        );
        assert!(stats.iter().all(|s| s.count == 1), "and measured exactly once");
        assert_eq!(run.totals.rounds, 1);
        assert_eq!(run.totals.records, 5);
    }

    #[tokio::test]
    async fn offsets_are_measured_against_the_original_quote_not_the_previous_block() {
        // The quote is 10_000 and every later block replays at 9_900. If an offset were measured
        // against the previous block instead of the quote, offsets 2+ would read 0 bps.
        let mut script = vec![(None, Some(10_000))];
        script.extend((1..=3).map(|_| (Some(9_900), Some(10_000))));
        let solver = ScriptedSolver::new(script);
        let pool = shapes(1);
        let mut run = state(3, pool.len());

        run_session(&solver, &config(1, 3, Some(3)), &pool, &mut run).await;

        for offset in run.summary.stats() {
            // -100 bps at every offset, all of it execution slippage (the fresh solve never moved).
            assert!(
                (offset.p50_bps + 100.0).abs() < 0.01,
                "offset {} read {} bps",
                offset.offset,
                offset.p50_bps
            );
            assert!((offset.execution_share - 1.0).abs() < 1e-9);
        }
    }

    #[tokio::test]
    async fn the_measured_block_advances_with_the_offset() {
        let mut script = vec![(None, Some(10_000))];
        script.extend((1..=3).map(|_| (Some(9_900), Some(9_900))));
        let solver = ScriptedSolver::new(script);
        let pool = shapes(1);
        let mut run = state(3, pool.len());

        run_session(&solver, &config(1, 3, Some(3)), &pool, &mut run).await;

        // The scripted solver reports block 1000+step, so a quote at 1000 must be measured at
        // 1001..1003 — a stuck measured_block would mean the loop reads the state before advancing.
        assert_eq!(solver.current_block().await, Some(1_003));
    }

    #[tokio::test]
    async fn a_dead_feed_mid_round_abandons_it_rather_than_mixing_states() {
        // The script dies after two offsets, so the round cannot finish.
        let script =
            vec![(None, Some(10_000)), (Some(9_900), Some(9_950)), (Some(9_800), Some(9_900))];
        let solver = ScriptedSolver::new(script);
        let pool = shapes(1);
        let mut run = state(5, pool.len());

        let end = run_session(&solver, &config(1, 5, None), &pool, &mut run).await;
        assert!(matches!(end, SessionEnd::Unhealthy(_)), "a dead feed must end the session");
        assert_eq!(run.totals.abandoned_rounds, 1);
        // Only the offsets that completed before the feed died are recorded.
        assert_eq!(
            run.summary
                .stats()
                .iter()
                .map(|s| s.offset)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn an_unquotable_sample_skips_the_round_without_measuring() {
        // No quote at block 0, so there is nothing to replay.
        let mut script = vec![(None, None)];
        script.extend((1..=5).map(|_| (Some(9_900), Some(9_950))));
        let solver = ScriptedSolver::new(script);
        let pool = shapes(2);
        let mut run = state(2, pool.len());

        let end = run_session(&solver, &config(2, 2, None), &pool, &mut run).await;
        // The round is skipped, then the next round quotes fine and runs until the script ends.
        assert!(matches!(end, SessionEnd::Unhealthy(_)));
        assert_eq!(run.totals.unquotable, 2, "both sampled shapes failed to quote");
        assert!(run.totals.rounds >= 1);
    }

    #[tokio::test]
    async fn a_failed_replay_is_counted_and_leaves_no_measurement() {
        let script = vec![(None, Some(10_000)), (None, Some(9_950)), (Some(9_900), Some(9_950))];
        let solver = ScriptedSolver::new(script);
        let pool = shapes(1);
        let mut run = state(2, pool.len());

        run_session(&solver, &config(1, 2, Some(2)), &pool, &mut run).await;

        // Offset 1's replay failed; offset 2's succeeded.
        assert_eq!(run.totals.simulation_failed, 1);
        assert_eq!(
            run.summary
                .stats()
                .iter()
                .map(|s| s.offset)
                .collect::<Vec<_>>(),
            vec![2]
        );
        // The record still exists — a missing measurement stays attributable.
        assert_eq!(run.totals.records, 2);
    }

    #[tokio::test]
    async fn quoting_draws_from_the_sample_pool() {
        let mut script = vec![(None, Some(10_000))];
        script.extend((1..=2).map(|_| (Some(9_900), Some(9_950))));
        let solver = ScriptedSolver::new(script);
        let pool = shapes(8);
        let mut run = state(2, pool.len());

        run_session(&solver, &config(3, 2, Some(2)), &pool, &mut run).await;

        let solved = solver.solved.lock().expect("lock");
        // 3 quotes at block 0, then 3 fresh solves at each of 2 offsets.
        assert_eq!(solved.len(), 9);
        let quoted_amounts: Vec<U256> = solved[..3]
            .iter()
            .map(|(_, amount)| *amount)
            .collect();
        assert!(
            quoted_amounts.iter().all(|amount| pool
                .iter()
                .any(|s| s.amount_in == *amount)),
            "every quote must come from the pool"
        );
        // The same three trades are re-solved at each offset, so the market-movement reference is
        // for the trade it is compared against.
        assert_eq!(
            solved[3..6]
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>(),
            solved[..3]
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
        );
    }
}
