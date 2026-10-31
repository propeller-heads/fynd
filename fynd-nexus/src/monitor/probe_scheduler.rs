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

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use rstest::rstest;

    use super::*;
    use crate::monitor::component_health::{HealthThresholds, Observation};

    const THRESHOLDS: HealthThresholds =
        HealthThresholds { broken_after: 2, inaccurate_after: 3, inaccuracy_bps: 100.0 };
    const REFRESH: Duration = Duration::from_secs(6 * 3600);
    const MAX_BACKOFF: Duration = Duration::from_secs(3600);

    fn scheduler() -> ProbeScheduler {
        ProbeScheduler::new(REFRESH, MAX_BACKOFF)
    }

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0)
            .unwrap()
    }

    fn seconds(count: i64) -> chrono::Duration {
        chrono::Duration::seconds(count)
    }

    fn new_component(queued_at: DateTime<Utc>) -> ComponentHealth {
        ComponentHealth::new("uniswap_v3".to_string(), queued_at)
    }

    fn healthy(checked_at: DateTime<Utc>) -> ComponentHealth {
        let mut health = new_component(start());
        let observation = Observation::Executed { block: 1, deviation_bps: 0.0 };
        health.record(observation, &THRESHOLDS, checked_at);
        health
    }

    fn reverted(checked_at: DateTime<Utc>, times: u64) -> ComponentHealth {
        let mut health = new_component(start());
        for block in 1..=times {
            let observation = Observation::PoolReverted { block, reason: "x".to_string() };
            health.record(observation, &THRESHOLDS, checked_at);
        }
        health
    }

    fn components(entries: Vec<(&str, ComponentHealth)>) -> BTreeMap<ComponentId, ComponentHealth> {
        entries
            .into_iter()
            .map(|(id, health)| (id.to_string(), health))
            .collect()
    }

    #[test]
    fn test_order_new_then_retry_then_refresh() {
        let now = start() + seconds(7 * 3600);
        let mut records = components(vec![
            ("0xa", healthy(start())),
            ("0xb", reverted(start(), 1)),
            ("0xc", new_component(start())),
        ]);
        let mut order = Vec::new();

        while let Some(id) = scheduler().next_to_probe(&records, now) {
            records.get_mut(&id).unwrap().record(
                Observation::Executed { block: 9, deviation_bps: 0.0 },
                &THRESHOLDS,
                now,
            );
            order.push(id);
        }

        assert_eq!(order, ["0xc", "0xb", "0xa"]);
    }

    #[test]
    fn test_new_components_latest_to_join_first() {
        let records = components(vec![
            ("0xa", new_component(start())),
            ("0xb", new_component(start() + seconds(60))),
            ("0xc", new_component(start() + seconds(30))),
        ]);

        let next = scheduler().next_to_probe(&records, start() + seconds(120));

        assert_eq!(next.as_deref(), Some("0xb"));
    }

    #[test]
    fn test_refresh_oldest_first() {
        let records = components(vec![
            ("0xa", healthy(start() + seconds(100))),
            ("0xb", healthy(start())),
            ("0xc", healthy(start() + seconds(50))),
        ]);

        let next = scheduler().next_to_probe(&records, start() + REFRESH + seconds(1000));

        assert_eq!(next.as_deref(), Some("0xb"));
    }

    #[rstest]
    #[case::first_retry(1, 60)]
    #[case::second_retry(2, 120)]
    #[case::fourth_retry(4, 480)]
    #[case::capped(20, 3600)]
    #[case::far_beyond_the_cap(500, 3600)]
    fn test_retry_backoff_doubles_up_to_the_maximum(
        #[case] unsettled_checks: u32,
        #[case] wait_seconds: i64,
    ) {
        let mut health = reverted(start(), 1);
        health.unsettled_checks = unsettled_checks;

        let due = scheduler().next_check_at(&health);

        assert_eq!(due, Some(start() + seconds(wait_seconds)));
    }

    #[test]
    fn test_nothing_is_picked_before_it_is_due() {
        let records = components(vec![("0xa", healthy(start())), ("0xb", reverted(start(), 1))]);

        let early = scheduler().next_to_probe(&records, start() + seconds(59));
        let retry_due = scheduler().next_to_probe(&records, start() + seconds(60));

        assert_eq!(early, None);
        assert_eq!(retry_due.as_deref(), Some("0xb"));
    }

    #[test]
    fn test_component_that_cannot_be_probed_is_skipped() {
        let mut skipped = new_component(start());
        skipped.mark_not_probeable(start());
        let records = components(vec![("0xa", skipped)]);

        let next = scheduler().next_to_probe(&records, start() + seconds(10 * 3600));

        assert_eq!(next, None);
        assert_eq!(scheduler().next_check_at(&records["0xa"]), None);
    }

    #[test]
    fn test_clean_component_that_could_not_be_probed_is_retried_with_backoff() {
        let mut health = healthy(start());
        health.record(
            Observation::SetupFailed { reason: "rpc down".to_string() },
            &THRESHOLDS,
            start() + seconds(10),
        );

        let due = scheduler().next_check_at(&health);

        assert_eq!(due, Some(start() + seconds(10 + 60)));
    }

    #[test]
    fn test_reset_component_goes_before_older_new_ones() {
        let mut records =
            components(vec![("0xa", new_component(start())), ("0xb", reverted(start(), 2))]);
        let now = start() + seconds(300);
        records
            .get_mut("0xb")
            .unwrap()
            .reset(now);

        let next = scheduler().next_to_probe(&records, now);

        assert_eq!(next.as_deref(), Some("0xb"));
    }
}
