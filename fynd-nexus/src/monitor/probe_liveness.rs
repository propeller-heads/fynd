//! Tells whether probing has stalled, and withdraws the excluded components when it has.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use metrics::gauge;
use tokio::time::Instant;
use tracing::warn;

use crate::monitor::excluded_publisher::ExcludedPublisher;

/// The shortest time without progress that counts as a stall.
const MIN_STALL: Duration = Duration::from_secs(5 * 60);

/// How many probe intervals without progress count as a stall, if that is longer than
/// [`MIN_STALL`].
const STALL_INTERVALS: u32 = 10;

/// How often the watcher checks for a stall and reports the age of the last probe.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// When the prober last made progress.
struct Progress {
    started: Instant,
    last_probe: Option<Instant>,
    last_idle: Option<Instant>,
}

/// Records when probing makes progress, and says when it has stopped.
///
/// Probing makes progress when a probe gives a verdict (the swap executed or reverted), and when
/// no component is due, because then there is nothing to do. It stalls when neither has happened
/// for ten probe intervals, and for at least five minutes.
pub struct ProbeLiveness {
    stall_after: Duration,
    progress: Mutex<Progress>,
}

impl ProbeLiveness {
    /// A tracker for a prober that starts a probe every `probe_interval`, started now.
    pub fn new(probe_interval: Duration) -> Self {
        Self {
            stall_after: (probe_interval * STALL_INTERVALS).max(MIN_STALL),
            progress: Mutex::new(Progress {
                started: Instant::now(),
                last_probe: None,
                last_idle: None,
            }),
        }
    }

    /// Records that a probe gave a verdict.
    pub fn record_probe(&self) {
        self.lock().last_probe = Some(Instant::now());
    }

    /// Records that no component was due.
    pub fn record_idle(&self) {
        self.lock().last_idle = Some(Instant::now());
    }

    /// Whether the prober made no progress for longer than the stall time.
    pub fn is_stalled(&self) -> bool {
        self.since_progress() > self.stall_after
    }

    /// How long ago the last probe gave a verdict, or since the start if none has.
    pub fn since_last_probe(&self) -> Duration {
        let progress = self.lock();
        progress
            .last_probe
            .unwrap_or(progress.started)
            .elapsed()
    }

    fn since_progress(&self) -> Duration {
        let progress = self.lock();
        let latest = [Some(progress.started), progress.last_probe, progress.last_idle]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(progress.started);
        latest.elapsed()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Withdraws the excluded components while `liveness` says probing has stalled, publishes them
/// again once it resumes, and keeps `nexus_last_probe_age_seconds` current.
pub async fn watch_liveness(
    liveness: Arc<ProbeLiveness>,
    publisher: Arc<ExcludedPublisher>,
    chain_label: String,
) {
    let mut ticker = tokio::time::interval(WATCH_INTERVAL);
    let mut stalled = false;
    loop {
        ticker.tick().await;
        gauge!("nexus_last_probe_age_seconds", "chain" => chain_label.clone()).set(
            liveness
                .since_last_probe()
                .as_secs_f64(),
        );
        let now_stalled = liveness.is_stalled();
        if now_stalled {
            publisher.withdraw();
            if !stalled {
                warn!("probing has stalled; withdrawing the excluded components");
            }
        } else if stalled {
            publisher.sync();
        }
        stalled = now_stalled;
    }
}
