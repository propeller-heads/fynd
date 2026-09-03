//! Worker-session supervision: the respawn policy and the per-thread session loop.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use tokio::sync::broadcast;
use tracing::error;

use crate::{
    algorithm::AlgorithmConfig,
    derived::{events::DerivedDataEvent, SharedDerivedDataRef},
    feed::{events::MarketEvent, market_data::MarketData},
    propamm_fallback::SharedFeeTiers,
    types::internal::SolveTask,
    worker_pool::worker::SolverWorker,
    worker_pool_router::LiquidityScope,
};

/// Retry policy for respawning a panicked worker.
///
/// Backoff doubles per consecutive failure up to `max_backoff` (the same
/// doubling-with-cap shape as the Rust client's `RetryConfig`). After
/// `max_attempts` consecutive failures the worker gives up. A session that
/// lives at least `stable_session` resets the budget, so spaced transient
/// panics respawn indefinitely while deterministic failures stop fast.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RespawnPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_attempts: u32,
    pub stable_session: Duration,
}

impl Default for RespawnPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            max_attempts: 10,
            stable_session: Duration::from_secs(600),
        }
    }
}

/// What the supervision loop does after a session panicked.
#[derive(Debug, PartialEq)]
pub(crate) enum FailureAction {
    /// Sleep this long, then respawn the worker.
    Retry(Duration),
    /// Stop respawning this worker.
    GiveUp,
}

/// Tracks consecutive session failures against a [`RespawnPolicy`].
pub(crate) struct RespawnState {
    policy: RespawnPolicy,
    consecutive_failures: u32,
    next_backoff: Duration,
}

impl RespawnState {
    pub(crate) fn new(policy: RespawnPolicy) -> Self {
        Self { policy, consecutive_failures: 0, next_backoff: policy.initial_backoff }
    }

    /// Records a panicked session that lived `session_lived` and decides the next action.
    pub(crate) fn on_failure(&mut self, session_lived: Duration) -> FailureAction {
        if session_lived >= self.policy.stable_session {
            self.consecutive_failures = 0;
            self.next_backoff = self.policy.initial_backoff;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.policy.max_attempts {
            return FailureAction::GiveUp;
        }
        let delay = self.next_backoff;
        self.next_backoff = (self.next_backoff * 2).min(self.policy.max_backoff);
        FailureAction::Retry(delay)
    }
}

/// Everything one worker thread needs to run sessions until shutdown or give-up.
pub(crate) struct WorkerContext<A, F>
where
    A: crate::algorithm::Algorithm + 'static,
    A::GraphManager:
        crate::feed::events::MarketEventHandler + crate::graph::EdgeWeightUpdaterWithDerived,
    F: Fn(AlgorithmConfig) -> A + Send + Sync + 'static,
{
    pub worker_id: usize,
    pub algorithm_name: String,
    pub pool_name: String,
    pub factory: F,
    pub algorithm_config: AlgorithmConfig,
    pub market_data: MarketData,
    pub derived_data: SharedDerivedDataRef,
    pub task_rx: async_channel::Receiver<SolveTask>,
    pub event_rx: broadcast::Receiver<MarketEvent>,
    pub derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    pub shutdown_rx: broadcast::Receiver<()>,
    pub liquidity_scope: LiquidityScope,
    pub exclude_protocols: Vec<String>,
    pub fallback_fee_tiers: SharedFeeTiers,
    pub respawn_policy: RespawnPolicy,
    pub on_worker_gave_up: Arc<dyn Fn() + Send + Sync>,
}

impl<A, F> WorkerContext<A, F>
where
    A: crate::algorithm::Algorithm + 'static,
    A::GraphManager:
        crate::feed::events::MarketEventHandler + crate::graph::EdgeWeightUpdaterWithDerived,
    F: Fn(AlgorithmConfig) -> A + Send + Sync + 'static,
{
    /// Runs worker sessions until clean shutdown or give-up.
    ///
    /// Panics (e.g. pool math dividing by zero) are contained to the current
    /// session so one bad task cannot permanently kill the worker thread.
    pub(crate) fn run_sessions(mut self) {
        let mut respawn = RespawnState::new(self.respawn_policy);
        loop {
            // Fresh receivers for this session; the previous session's receivers
            // were moved into it and dropped when it ended.
            let session_event_rx = self.event_rx.resubscribe();
            let session_derived_event_rx = self.derived_event_rx.resubscribe();
            let session_shutdown_rx = self.shutdown_rx.resubscribe();

            // A shutdown sent while no session was listening (e.g. mid-respawn) is
            // buffered in the receiver created before the thread started.
            match self.shutdown_rx.try_recv() {
                Err(broadcast::error::TryRecvError::Empty) => {}
                // Received a shutdown, or the pool dropped the sender.
                _ => break,
            }

            let session_started = Instant::now();
            let session_result = catch_unwind(AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime");

                rt.block_on(async {
                    let algorithm = (self.factory)(self.algorithm_config.clone());

                    let mut worker = SolverWorker::new(
                        self.market_data.clone(),
                        Arc::clone(&self.derived_data),
                        algorithm,
                        self.worker_id,
                        self.pool_name.clone(),
                    )
                    .with_liquidity_scope(self.liquidity_scope)
                    .with_exclude_protocols(self.exclude_protocols.clone())
                    .with_fallback_fee_tiers(self.fallback_fee_tiers.clone());

                    worker.initialize_graph().await;
                    worker
                        .run(
                            session_event_rx,
                            session_derived_event_rx,
                            self.task_rx.clone(),
                            session_shutdown_rx,
                        )
                        .await;
                });
            }));

            match session_result {
                Ok(()) => break,
                Err(panic_payload) => {
                    let panic_message = panic_payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| {
                            panic_payload
                                .downcast_ref::<String>()
                                .map(String::as_str)
                        })
                        .unwrap_or("<non-string panic payload>");
                    error!(
                        pool = %self.pool_name,
                        algorithm = %self.algorithm_name,
                        worker_id = self.worker_id,
                        panic = %panic_message,
                        "worker thread panicked; respawning worker"
                    );
                    metrics::counter!(
                        "worker_pool_worker_panics_total",
                        "pool" => self.pool_name.clone()
                    )
                    .increment(1);
                    match respawn.on_failure(session_started.elapsed()) {
                        FailureAction::Retry(delay) => thread::sleep(delay),
                        FailureAction::GiveUp => {
                            error!(
                                pool = %self.pool_name,
                                worker_id = self.worker_id,
                                "worker gave up after repeated rapid panics"
                            );
                            (self.on_worker_gave_up)();
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn fast_policy() -> RespawnPolicy {
        RespawnPolicy {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
            max_attempts: 3,
            stable_session: Duration::from_secs(60),
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut state = RespawnState::new(fast_policy());
        let lived = Duration::from_millis(1);
        assert_eq!(state.on_failure(lived), FailureAction::Retry(Duration::from_millis(100)));
        assert_eq!(state.on_failure(lived), FailureAction::Retry(Duration::from_millis(200)));
        assert_eq!(state.on_failure(lived), FailureAction::GiveUp);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let mut state = RespawnState::new(RespawnPolicy { max_attempts: 1, ..fast_policy() });
        assert_eq!(state.on_failure(Duration::from_millis(1)), FailureAction::GiveUp);
    }

    #[test]
    fn stable_session_resets_the_budget() {
        let mut state = RespawnState::new(fast_policy());
        let rapid = Duration::from_millis(1);
        state.on_failure(rapid);
        state.on_failure(rapid);
        // A session that lived past the stability threshold resets attempts and backoff.
        assert_eq!(
            state.on_failure(Duration::from_secs(61)),
            FailureAction::Retry(Duration::from_millis(100))
        );
    }

    #[test]
    fn backoff_cap_bounds_the_delay() {
        let mut state = RespawnState::new(RespawnPolicy { max_attempts: 10, ..fast_policy() });
        let rapid = Duration::from_millis(1);
        let mut last = Duration::ZERO;
        for _ in 0..5 {
            if let FailureAction::Retry(d) = state.on_failure(rapid) {
                last = d;
            }
        }
        assert_eq!(last, Duration::from_millis(400));
    }
}
