//! The wall clock of the monitor, driven by Tokio's clock.

use chrono::{DateTime, Utc};
use tokio::time::Instant;

/// Tells the time as a date, advancing with Tokio's clock.
///
/// The health records and the retry schedule are dates that survive a restart. Reading the date
/// from Tokio's clock lets tests pause and advance the time the monitor sees.
#[derive(Debug, Clone, Copy)]
pub struct MonitorClock {
    started: Instant,
    started_at: DateTime<Utc>,
}

impl MonitorClock {
    /// A clock that reads the current date now and advances with Tokio's clock after that.
    pub fn new() -> Self {
        Self { started: Instant::now(), started_at: Utc::now() }
    }

    /// The current date.
    pub fn now(&self) -> DateTime<Utc> {
        let elapsed = chrono::Duration::from_std(self.started.elapsed())
            .unwrap_or_else(|_| chrono::Duration::zero());
        self.started_at + elapsed
    }
}

impl Default for MonitorClock {
    fn default() -> Self {
        Self::new()
    }
}
