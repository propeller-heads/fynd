//! Restarts a solver worker whose loop panicked.
//!
//! Each [`SolverWorker`](super::worker::SolverWorker) runs on its own OS thread. A panic there
//! unwinds only that thread: the pool keeps serving on whatever workers are left, and once every
//! worker is gone the pool's task receiver closes and enqueueing fails with
//! [`SolveError::QueueFull`](crate::SolveError::QueueFull). Nothing reports either state, because
//! the per-pool metrics are emitted by the workers themselves.
//!
//! [`run_supervised`] keeps the thread alive instead. It re-runs the worker loop after a panic and
//! waits longer before each successive restart, so a panic that reproduces on every order backs off
//! rather than spinning, and gives up once restarting has stopped helping.

use std::{
    any::Any,
    panic::{catch_unwind, AssertUnwindSafe},
    thread,
    time::{Duration, Instant},
};

use tokio::sync::broadcast;
use tracing::{error, warn};

/// How long to wait before the first restart.
const FIRST_BACKOFF: Duration = Duration::from_millis(100);

/// Longest wait between restarts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long a worker must stay up for its next panic to count as a fresh failure rather than a
/// repeat of the one before it.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Repeat panics before the worker is given up on.
const MAX_CONSECUTIVE_PANICS: u32 = 8;

/// How often the backoff wait checks whether shutdown was signalled.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Names one worker thread in logs and metrics.
pub(crate) struct WorkerIdentity {
    /// Worker pool name from configuration, matching the `pool` metric label.
    pub pool: String,
    /// Algorithm the pool runs.
    pub algorithm: String,
    /// Index of this worker within its pool.
    pub worker_id: usize,
}

/// Runs `run_once` until it returns without panicking, restarting it after a panic with backoff.
///
/// `run_once` receives a fresh shutdown receiver on every attempt and is expected to block for the
/// life of one worker loop. This returns when that loop exits on its own — shutdown, or a closed
/// event channel — when shutdown is signalled while backing off, or when `MAX_CONSECUTIVE_PANICS`
/// restarts in a row have failed to produce a worker that stayed up for `HEALTHY_RUN`.
pub(crate) fn run_supervised<F>(
    identity: &WorkerIdentity,
    shutdown_tx: &broadcast::Sender<()>,
    mut run_once: F,
) where
    F: FnMut(broadcast::Receiver<()>),
{
    let mut supervisor_shutdown_rx = shutdown_tx.subscribe();
    let mut consecutive_panics = 0;
    let mut backoff = FIRST_BACKOFF;

    loop {
        // Subscribe before checking, so a shutdown racing this attempt is seen by one of the two
        // receivers rather than by neither.
        let worker_shutdown_rx = shutdown_tx.subscribe();
        if shutdown_signalled(&mut supervisor_shutdown_rx) {
            return;
        }

        let started = Instant::now();
        let Err(payload) = catch_unwind(AssertUnwindSafe(|| run_once(worker_shutdown_rx))) else {
            return;
        };
        let uptime = started.elapsed();

        if uptime >= HEALTHY_RUN {
            consecutive_panics = 0;
            backoff = FIRST_BACKOFF;
        }
        consecutive_panics += 1;

        metrics::counter!(
            "worker_pool_worker_panics_total",
            "pool" => identity.pool.clone(),
            "algorithm" => identity.algorithm.clone(),
        )
        .increment(1);

        error!(
            pool = %identity.pool,
            algorithm = %identity.algorithm,
            worker_id = identity.worker_id,
            uptime_ms = uptime.as_millis() as u64,
            consecutive_panics,
            panic = %panic_message(payload.as_ref()),
            "solver worker panicked"
        );

        if consecutive_panics >= MAX_CONSECUTIVE_PANICS {
            error!(
                pool = %identity.pool,
                algorithm = %identity.algorithm,
                worker_id = identity.worker_id,
                consecutive_panics,
                "giving up on solver worker; the pool now serves on its remaining workers"
            );
            return;
        }

        warn!(
            pool = %identity.pool,
            algorithm = %identity.algorithm,
            worker_id = identity.worker_id,
            backoff_ms = backoff.as_millis() as u64,
            "restarting solver worker"
        );

        if wait_for_backoff(&mut supervisor_shutdown_rx, backoff) {
            return;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Whether shutdown has been signalled on `shutdown_rx`.
///
/// A closed channel means every sender is gone, and a lagged one means a signal was sent and
/// missed; both are treated as shutdown, since neither leaves anything to keep running for.
fn shutdown_signalled(shutdown_rx: &mut broadcast::Receiver<()>) -> bool {
    match shutdown_rx.try_recv() {
        Ok(()) => true,
        Err(broadcast::error::TryRecvError::Closed) => true,
        Err(broadcast::error::TryRecvError::Lagged(_)) => true,
        Err(broadcast::error::TryRecvError::Empty) => false,
    }
}

/// Sleeps for `backoff`, returning true if shutdown was signalled before it elapsed.
fn wait_for_backoff(shutdown_rx: &mut broadcast::Receiver<()>, backoff: Duration) -> bool {
    let deadline = Instant::now() + backoff;

    loop {
        if shutdown_signalled(shutdown_rx) {
            return true;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(remaining.min(SHUTDOWN_POLL));
    }
}

/// The message a panic carried, for logging.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return message;
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message;
    }
    "panic payload was not a string"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> WorkerIdentity {
        WorkerIdentity {
            pool: "test_pool".to_string(),
            algorithm: "most_liquid".to_string(),
            worker_id: 0,
        }
    }

    #[test]
    fn test_run_supervised_restarts_after_panic() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let mut attempts = 0;

        run_supervised(&test_identity(), &shutdown_tx, |_shutdown_rx| {
            attempts += 1;
            assert!(attempts > 1, "first attempt panics on purpose");
        });

        assert_eq!(attempts, 2);
    }

    #[test]
    fn test_run_supervised_returns_when_worker_exits() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let mut attempts = 0;

        run_supervised(&test_identity(), &shutdown_tx, |_shutdown_rx| {
            attempts += 1;
        });

        assert_eq!(attempts, 1);
    }

    #[test]
    fn test_run_supervised_skips_worker_when_already_shut_down() {
        let (shutdown_tx, _keep_open) = broadcast::channel(1);
        shutdown_tx
            .send(())
            .expect("receiver is held open by the test");
        let mut attempts = 0;

        run_supervised(&test_identity(), &shutdown_tx, |_shutdown_rx| {
            attempts += 1;
        });

        assert_eq!(attempts, 0);
    }

    #[test]
    fn test_shutdown_signalled_on_empty_and_closed() {
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);
        assert!(!shutdown_signalled(&mut shutdown_rx));

        drop(shutdown_tx);
        assert!(shutdown_signalled(&mut shutdown_rx));
    }

    #[test]
    fn test_panic_message_reads_both_payload_shapes() {
        let literal: Box<dyn Any + Send> = Box::new("literal panic");
        assert_eq!(panic_message(literal.as_ref()), "literal panic");

        let formatted: Box<dyn Any + Send> = Box::new("formatted panic".to_string());
        assert_eq!(panic_message(formatted.as_ref()), "formatted panic");

        let neither: Box<dyn Any + Send> = Box::new(7_u32);
        assert_eq!(panic_message(neither.as_ref()), "panic payload was not a string");
    }
}
