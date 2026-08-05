//! The APEX batch stage: a worker pool that solves batch jobs off the block loop's critical path.
//!
//! The monitor's per-block pipeline only *dispatches* here — at the pre-advance seam for the
//! top-bracket job (cloned N−1 inputs) and after the backs for the bottom bracket — and drains
//! results asynchronously. Three properties the design guarantees, each grill-hardened:
//!
//! - **The block loop never waits.** Dispatch is `try_send` on a bounded queue: a full queue drops
//!   the job and counts it (`apex_skipped{queue_full}`) instead of stalling the loop or growing
//!   without bound. Jobs carry inputs cloned at block time, so queue delay affects only metric
//!   latency, never state parity.
//! - **The budget is a solve budget, not a queue budget.** APEX's deadline is an absolute
//!   `Instant`; computed at enqueue it can expire in the queue, making the solver return a silently
//!   *empty* result on exactly the busy blocks that matter. Workers therefore stamp the deadline
//!   when they *pick a job up*, and report the queue wait separately.
//! - **Overruns are visible, not fatal.** The deadline only bounds APEX's price search — the
//!   clearing phase runs unbounded — and a worker thread cannot be cancelled. A result arriving
//!   after [`OVERRUN_FACTOR`]× the budget is discarded and counted; the thread's occupancy loss
//!   shows up as queue depth, which is the honest signal.
//!
//! The solve function is a parameter: the stage owns threads, queueing, deadlines, and counters,
//! and knows nothing about APEX itself. The apex-batch library call plugs in at the wiring step.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use tracing::warn;

/// A solve result is discarded (and counted) when its wall time exceeded this multiple of the
/// budget — it describes a state too stale to compare against, and letting it through would bias
/// the sample toward slow, complex batches measured late.
pub(crate) const OVERRUN_FACTOR: u32 = 3;

/// Whether a dispatched job entered the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchOutcome {
    Queued,
    /// Dropped without solving; the reason is already counted.
    Skipped(SkipReason),
}

/// Why a job was dropped at dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The bounded queue was full — the workers are saturated and this block's job is shed
    /// rather than delaying every later block's.
    QueueFull,
    /// The job channel is disconnected — every worker has exited (or none was spawned) — so no
    /// amount of waiting would ever drain this job. Distinct from `QueueFull` because it signals
    /// the pool itself is gone, not merely busy.
    PoolGone,
}

/// Stage timing for one delivered solve.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SolveTiming {
    /// Dispatch to worker pickup — how long the job sat in the queue.
    pub queue_wait: Duration,
    /// Worker pickup to solve return — the whole solve, search and clearing phases both.
    pub solve_wall: Duration,
}

/// A solve result with its stage timing.
#[derive(Debug)]
pub(crate) struct StageDelivery<R> {
    pub result: R,
    pub timing: SolveTiming,
}

/// The stage's own counters. Prometheus wiring reads these through
/// [`crate::telemetry::record_apex_dispatch`] and friends at the call sites; the atomics exist so
/// the stage is testable without a metrics recorder.
#[derive(Debug, Default)]
pub(crate) struct StageCounters {
    pub dispatched: AtomicU64,
    pub skipped_queue_full: AtomicU64,
    pub skipped_pool_gone: AtomicU64,
    pub delivered: AtomicU64,
    pub overruns: AtomicU64,
    /// Solves that panicked. The job is dropped and the worker keeps running — see `worker_loop`.
    pub panics: AtomicU64,
}

struct QueuedJob<J> {
    payload: J,
    enqueued_at: Instant,
}

/// One worker: drain jobs until the stage (or the delivery receiver) is dropped.
fn worker_loop<J, R, F>(
    job_receiver: &Mutex<Receiver<QueuedJob<J>>>,
    deliveries: &std::sync::mpsc::Sender<StageDelivery<R>>,
    solve: &F,
    counters: &StageCounters,
    budget: Duration,
) where
    F: Fn(J, Instant) -> R,
{
    loop {
        // Hold the lock only for the blocking recv; solving happens unlocked so the other
        // worker keeps draining.
        let job = {
            let Ok(receiver) = job_receiver.lock() else {
                return;
            };
            match receiver.recv() {
                Ok(job) => job,
                Err(_) => return, // stage dropped: clean shutdown
            }
        };
        let picked_up = Instant::now();
        let queue_wait = picked_up.saturating_duration_since(job.enqueued_at);
        // The deadline starts NOW — a job that waited in the queue keeps its full solve budget.
        let deadline = picked_up + budget;
        // A panicking solve (APEX's own price search, or pool math it calls into) must not take
        // the worker thread down with it — that would permanently shrink the pool by one, silently.
        let result = match catch_unwind(AssertUnwindSafe(|| solve(job.payload, deadline))) {
            Ok(result) => result,
            Err(panic_payload) => {
                let message = panic_payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| {
                        panic_payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                    })
                    .unwrap_or("<non-string panic payload>");
                warn!(panic = message, "APEX solve panicked; job dropped, worker continues");
                counters
                    .panics
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let solve_wall = picked_up.elapsed();
        if solve_wall > budget * OVERRUN_FACTOR {
            counters
                .overruns
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if deliveries
            .send(StageDelivery { result, timing: SolveTiming { queue_wait, solve_wall } })
            .is_err()
        {
            return; // nobody is draining anymore
        }
        counters
            .delivered
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// A fixed pool of OS threads solving batch jobs from a bounded queue.
///
/// Owned threads, not `tokio::spawn_blocking`: tokio's blocking pool is runtime-wide (shared
/// with JSONL IO and capped only globally), so it can neither bound this stage's parallelism nor
/// protect the async workers from it. Two dedicated threads make the stage's CPU ceiling explicit.
pub(crate) struct ApexStage<J: Send + 'static> {
    jobs: SyncSender<QueuedJob<J>>,
    workers: Vec<JoinHandle<()>>,
    counters: Arc<StageCounters>,
}

impl<J: Send + 'static> ApexStage<J> {
    /// Spawn `workers` threads solving with `solve`, queueing at most `queue_capacity` jobs.
    ///
    /// `solve` receives the job and the deadline — computed at solve start, `budget` from the
    /// moment the worker picked the job up. Returns the stage and the receiver the monitor
    /// drains; results whose solve exceeded [`OVERRUN_FACTOR`]`× budget` are discarded and
    /// counted instead of delivered.
    pub(crate) fn spawn<R, F>(
        workers: usize,
        queue_capacity: usize,
        budget: Duration,
        solve: F,
    ) -> (Self, Receiver<StageDelivery<R>>)
    where
        R: Send + 'static,
        F: Fn(J, Instant) -> R + Send + Sync + 'static,
    {
        let (jobs, job_receiver) = sync_channel::<QueuedJob<J>>(queue_capacity);
        let (deliveries, delivery_receiver) = std::sync::mpsc::channel::<StageDelivery<R>>();
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let solve = Arc::new(solve);
        let counters = Arc::new(StageCounters::default());

        let handles = (0..workers)
            .map(|_| {
                let job_receiver = Arc::clone(&job_receiver);
                let deliveries = deliveries.clone();
                let solve = Arc::clone(&solve);
                let counters = Arc::clone(&counters);
                std::thread::spawn(move || {
                    worker_loop(&job_receiver, &deliveries, solve.as_ref(), &counters, budget);
                })
            })
            .collect();

        (Self { jobs, workers: handles, counters }, delivery_receiver)
    }

    /// Queue a job, or shed it when the queue is full. Never blocks.
    pub(crate) fn dispatch(&self, payload: J) -> DispatchOutcome {
        match self
            .jobs
            .try_send(QueuedJob { payload, enqueued_at: Instant::now() })
        {
            Ok(()) => {
                self.counters
                    .dispatched
                    .fetch_add(1, Ordering::Relaxed);
                DispatchOutcome::Queued
            }
            Err(TrySendError::Full(_)) => {
                self.counters
                    .skipped_queue_full
                    .fetch_add(1, Ordering::Relaxed);
                DispatchOutcome::Skipped(SkipReason::QueueFull)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters
                    .skipped_pool_gone
                    .fetch_add(1, Ordering::Relaxed);
                DispatchOutcome::Skipped(SkipReason::PoolGone)
            }
        }
    }

    /// The stage's counters, for metrics recording and tests.
    pub(crate) fn counters(&self) -> &StageCounters {
        &self.counters
    }

    /// Stop accepting jobs, finish what is queued, and join the workers.
    pub(crate) fn shutdown(self) {
        drop(self.jobs);
        for worker in self.workers {
            // `worker_loop` catches solve panics itself (counted in `panics`), so the thread
            // returns normally even after one; a join `Err` here would mean a panic outside that
            // boundary, which shutdown can only reap, not recover from.
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::RecvTimeoutError;

    use super::*;

    const BUDGET: Duration = Duration::from_millis(40);

    /// Timing the solve fn observed, sent back through the result so tests can assert the
    /// deadline semantics from inside the worker.
    struct Observed {
        job: u32,
        budget_at_start: Duration,
    }

    #[test]
    fn test_full_queue_sheds_and_counts() {
        // One worker held busy by a long first job; capacity-1 queue. The second job queues,
        // the third is shed.
        let (stage, deliveries) = ApexStage::spawn(1, 1, BUDGET, |job: u32, _deadline| {
            if job == 0 {
                std::thread::sleep(Duration::from_millis(30));
            }
            job
        });
        assert_eq!(stage.dispatch(0), DispatchOutcome::Queued);
        // Give the worker time to pick up job 0 so the queue is empty for job 1.
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(stage.dispatch(1), DispatchOutcome::Queued);
        assert_eq!(stage.dispatch(2), DispatchOutcome::Skipped(SkipReason::QueueFull));
        assert_eq!(
            stage
                .counters()
                .skipped_queue_full
                .load(Ordering::Relaxed),
            1
        );

        let delivered: Vec<u32> = [&deliveries, &deliveries]
            .iter()
            .map(|rx| {
                rx.recv_timeout(Duration::from_secs(2))
                    .expect("both queued jobs deliver")
                    .result
            })
            .collect();
        assert_eq!(delivered, vec![0, 1]);
        stage.shutdown();
    }

    #[test]
    fn test_deadline_starts_at_pickup_not_enqueue() {
        // Job 0 occupies the single worker; job 1 waits in the queue. When job 1 finally runs,
        // its deadline must still be a full budget away — a deadline stamped at enqueue would
        // already be half spent.
        let (stage, deliveries) = ApexStage::spawn(1, 2, BUDGET, |job: u32, deadline| {
            let budget_at_start = deadline.saturating_duration_since(Instant::now());
            if job == 0 {
                std::thread::sleep(Duration::from_millis(25));
            }
            Observed { job, budget_at_start }
        });
        stage.dispatch(0);
        stage.dispatch(1);

        let mut queued_job_delivery = None;
        for _ in 0..2 {
            let delivery = deliveries
                .recv_timeout(Duration::from_secs(2))
                .expect("both jobs deliver");
            assert!(
                delivery.result.budget_at_start >= BUDGET.saturating_sub(Duration::from_millis(5)),
                "job {} started with only {:?} of its {BUDGET:?} budget",
                delivery.result.job,
                delivery.result.budget_at_start
            );
            if delivery.result.job == 1 {
                queued_job_delivery = Some(delivery);
            }
        }
        let queued = queued_job_delivery.expect("job 1 delivered");
        assert!(
            queued.timing.queue_wait >= Duration::from_millis(15),
            "job 1 sat behind job 0; measured wait {:?}",
            queued.timing.queue_wait
        );
        stage.shutdown();
    }

    #[test]
    fn test_overrun_result_discarded_and_counted() {
        let budget = Duration::from_millis(5);
        let (stage, deliveries) = ApexStage::spawn(1, 2, budget, |job: u32, _deadline| {
            if job == 0 {
                // > OVERRUN_FACTOR × budget: the result must be discarded.
                std::thread::sleep(Duration::from_millis(40));
            }
            job
        });
        stage.dispatch(0);
        stage.dispatch(1);

        let delivered = deliveries
            .recv_timeout(Duration::from_secs(2))
            .expect("the fast job delivers");
        assert_eq!(delivered.result, 1, "the overrun job must not be delivered");
        assert!(
            matches!(
                deliveries.recv_timeout(Duration::from_millis(50)),
                Err(RecvTimeoutError::Timeout)
            ),
            "nothing else arrives"
        );
        assert_eq!(
            stage
                .counters()
                .overruns
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            stage
                .counters()
                .delivered
                .load(Ordering::Relaxed),
            1
        );
        stage.shutdown();
    }

    #[test]
    fn test_panicking_solve_does_not_kill_worker() {
        // Job 0 panics; job 1 must still be picked up and delivered by the same worker thread.
        let (stage, deliveries) = ApexStage::spawn(1, 2, BUDGET, |job: u32, _deadline| {
            assert!(job != 0, "simulated solve panic");
            job
        });
        stage.dispatch(0);
        stage.dispatch(1);

        let delivered = deliveries
            .recv_timeout(Duration::from_secs(2))
            .expect("the surviving job still delivers");
        assert_eq!(delivered.result, 1, "the panicking job must not deliver a result");
        assert_eq!(
            stage
                .counters()
                .panics
                .load(Ordering::Relaxed),
            1
        );
        stage.shutdown();
    }

    #[test]
    fn test_disconnected_queue_counts_pool_gone() {
        // No workers spawned: the job receiver's only `Arc` reference lives inside `spawn`'s
        // local variable, so it drops as soon as `spawn` returns — before any dispatch — leaving
        // the channel disconnected rather than merely full.
        let (stage, _deliveries) = ApexStage::spawn(0, 1, BUDGET, |job: u32, _deadline| job);
        assert_eq!(stage.dispatch(0), DispatchOutcome::Skipped(SkipReason::PoolGone));
        assert_eq!(
            stage
                .counters()
                .skipped_pool_gone
                .load(Ordering::Relaxed),
            1
        );
        stage.shutdown();
    }

    #[test]
    fn test_shutdown_drains_queue_and_joins() {
        let (stage, deliveries) = ApexStage::spawn(2, 8, BUDGET, |job: u32, _deadline| job * 2);
        for job in 0..6 {
            assert_eq!(stage.dispatch(job), DispatchOutcome::Queued);
        }
        stage.shutdown(); // must finish queued work and join without hanging

        let mut results: Vec<u32> = deliveries
            .iter()
            .map(|d| d.result)
            .collect();
        results.sort_unstable();
        assert_eq!(results, vec![0, 2, 4, 6, 8, 10]);
    }
}
