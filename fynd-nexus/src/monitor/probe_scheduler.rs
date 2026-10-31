//! Decides which component to probe next.

use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, Utc};
use fynd_core::ComponentId;

use crate::monitor::component_health::{ComponentHealth, HealthStatus};

/// The wait before the first retry of a component that was not clean. It doubles with every
/// further probe that was not clean, up to the maximum retry backoff.
const FIRST_RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Why a component is up for a probe, in the order the scheduler serves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProbeTier {
    /// The component was never probed since it joined or was reset.
    New,
    /// The last probe was not clean: a revert, a shortfall, or a probe that could not run.
    Retry,
    /// The component was clean, and its last probe is old.
    Refresh,
}

/// When a component is next due and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Due {
    tier: ProbeTier,
    at: DateTime<Utc>,
}

/// Picks the component to probe next, and says when each component is due.
///
/// In order:
/// 1. components never probed, the ones that joined last first;
/// 2. components whose last probe was not clean and whose backoff has passed, the earliest due
///    first. The backoff is one minute, doubles with every probe that was not clean, and stops at
///    the maximum retry backoff;
/// 3. clean components whose last probe is older than the refresh interval, the oldest first.
///
/// Components that cannot be probed are never picked.
#[derive(Debug, Clone, Copy)]
pub struct ProbeScheduler {
    refresh_interval: Duration,
    max_retry_backoff: Duration,
}

impl ProbeScheduler {
    /// A scheduler that probes clean components again after `refresh_interval`, and waits at
    /// most `max_retry_backoff` before retrying one that was not clean.
    pub fn new(refresh_interval: Duration, max_retry_backoff: Duration) -> Self {
        Self { refresh_interval, max_retry_backoff }
    }

    /// The component to probe at `now`, or `None` when none is due.
    pub fn next_to_probe(
        &self,
        components: &BTreeMap<ComponentId, ComponentHealth>,
        now: DateTime<Utc>,
    ) -> Option<ComponentId> {
        let mut best: Option<(ProbeTier, i64, &ComponentId)> = None;
        for (id, health) in components {
            let Some(due) = self.due(health) else {
                continue;
            };
            if due.at > now {
                continue;
            }
            let rank = match due.tier {
                ProbeTier::New => -due.at.timestamp_millis(),
                ProbeTier::Retry | ProbeTier::Refresh => due.at.timestamp_millis(),
            };
            let candidate = (due.tier, rank, id);
            if best.is_none_or(|current| candidate < current) {
                best = Some(candidate);
            }
        }
        best.map(|(_, _, id)| id.clone())
    }

    /// When the component is due for its next probe, or `None` if it cannot be probed.
    pub fn next_check_at(&self, health: &ComponentHealth) -> Option<DateTime<Utc>> {
        self.due(health).map(|due| due.at)
    }

    fn due(&self, health: &ComponentHealth) -> Option<Due> {
        let not_clean = match health.status {
            HealthStatus::NotProbeable => return None,
            HealthStatus::Suspect | HealthStatus::Broken | HealthStatus::Inaccurate => true,
            HealthStatus::Unknown | HealthStatus::Healthy => health.unsettled_checks > 0,
        };
        let Some(checked_at) = health.checked_at else {
            return Some(Due { tier: ProbeTier::New, at: health.queued_at });
        };
        if not_clean {
            let wait = self.retry_backoff(health.unsettled_checks);
            return Some(Due { tier: ProbeTier::Retry, at: checked_at + wait });
        }
        Some(Due { tier: ProbeTier::Refresh, at: checked_at + self.refresh_interval })
    }

    fn retry_backoff(&self, unsettled_checks: u32) -> Duration {
        let doublings = unsettled_checks
            .saturating_sub(1)
            .min(20);
        (FIRST_RETRY_BACKOFF * 2_u32.pow(doublings)).min(self.max_retry_backoff)
    }
}
