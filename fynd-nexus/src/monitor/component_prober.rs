//! The loop that probes the components, one at a time, at a fixed rate.

use std::{sync::Arc, time::Duration};

use metrics::{counter, histogram};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::monitor::{
    component_checker::{CheckResult, ComponentChecker},
    component_health::{ComponentHealth, HealthStatus, Observation},
    Monitor,
};

/// Probes the component the scheduler picks every `probe_interval`, and records what each probe
/// found.
///
/// A probe that outlasts the interval delays the next one instead of overlapping it. After a
/// probe that gave a verdict it publishes the excluded components if they changed. It counts the
/// probes in `nexus_probes_total` and the deviation of the executed ones in
/// `nexus_probe_deviation_bps`. Revert reasons go to the log, never to a label.
pub struct ComponentProber<Checker> {
    checker: Checker,
    monitor: Arc<Monitor>,
    probe_interval: Duration,
}

impl<Checker: ComponentChecker> ComponentProber<Checker> {
    /// A prober of the components of `monitor`, which starts a probe every `probe_interval`.
    pub fn new(checker: Checker, monitor: Arc<Monitor>, probe_interval: Duration) -> Self {
        Self { checker, monitor, probe_interval }
    }

    /// Probes for as long as the task runs.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.probe_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.probe_next().await;
        }
    }

    /// Probes the component that is due next, if there is one.
    pub async fn probe_next(&self) {
        let now = self.monitor.clock.now();
        let due = self
            .monitor
            .store
            .with_components(|components| {
                self.monitor
                    .scheduler
                    .next_to_probe(components, now)
            });
        let Some(component_id) = due else {
            self.monitor.liveness.record_idle();
            if self.monitor.store.has_verdicts() {
                self.monitor.publisher.sync();
            }
            return;
        };
        let Some(before) = self.monitor.store.get(&component_id) else {
            return;
        };
        let rotation = usize::try_from(before.probes).unwrap_or(usize::MAX);
        let result = self
            .checker
            .check(&component_id, rotation)
            .await;
        let protocol = before.protocol_system.as_str();
        match result {
            CheckResult::NotProbeable => {
                self.count(protocol, "not_probeable");
                debug!(%component_id, protocol, "the component cannot be probed");
                self.monitor
                    .store
                    .mark_not_probeable(&component_id, self.monitor.clock.now());
            }
            CheckResult::Observed(observation) => {
                self.count(protocol, observation.name());
                self.record(&component_id, protocol, before.status, observation);
            }
        }
    }

    fn record(
        &self,
        component_id: &str,
        protocol: &str,
        status_before: HealthStatus,
        observation: Observation,
    ) {
        let gave_verdict = match &observation {
            Observation::Executed { deviation_bps, .. } => {
                histogram!(
                    "nexus_probe_deviation_bps",
                    "chain" => self.monitor.chain.to_string(),
                    "protocol" => protocol.to_string()
                )
                .record(*deviation_bps);
                true
            }
            Observation::PoolReverted { reason, .. } |
            Observation::TokenReverted { reason, .. } => {
                debug!(%component_id, protocol, reason, "the probe reverted");
                true
            }
            Observation::SetupFailed { reason } => {
                warn!(%component_id, protocol, reason, "the probe could not run");
                false
            }
            Observation::BlockUnavailable => {
                debug!(%component_id, protocol, "the node does not have the block of the probe");
                false
            }
        };
        self.monitor
            .store
            .record(component_id, observation, self.monitor.clock.now());
        if let Some(after) = self.monitor.store.get(component_id) {
            log_status_change(component_id, protocol, status_before, &after);
        }
        if gave_verdict {
            self.monitor.liveness.record_probe();
            self.monitor.publisher.sync();
        }
    }

    fn count(&self, protocol: &str, outcome: &'static str) {
        counter!(
            "nexus_probes_total",
            "chain" => self.monitor.chain.to_string(),
            "protocol" => protocol.to_string(),
            "outcome" => outcome
        )
        .increment(1);
    }
}

fn log_status_change(
    component_id: &str,
    protocol: &str,
    before: HealthStatus,
    after: &ComponentHealth,
) {
    if before == after.status {
        return;
    }
    let reason = after.reason.as_deref().unwrap_or("");
    info!(%component_id, protocol, %before, after = %after.status, reason, "component status changed");
}
