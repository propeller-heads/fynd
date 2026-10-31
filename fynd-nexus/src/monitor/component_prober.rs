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

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap, VecDeque},
        sync::Mutex,
    };

    use fynd_core::nexus::{components::ExclusionCause, MessageBody, NexusMessage};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use tycho_simulation::evm::tycho_models::Chain;

    use super::*;
    use crate::{
        latest_stream::LatestStream,
        monitor::{
            component_checker::CheckResult, component_health::HealthThresholds,
            excluded_publisher::ExcludedPublisher, health_store::HealthStore,
            monitor_clock::MonitorClock, probe_liveness::ProbeLiveness,
            probe_scheduler::ProbeScheduler,
        },
        stream_kind::StreamKind,
    };

    const INTERVAL: Duration = Duration::from_millis(500);
    const THRESHOLDS: HealthThresholds =
        HealthThresholds { broken_after: 2, inaccurate_after: 3, inaccuracy_bps: 100.0 };

    /// Answers each check with the next scripted result of the component, and remembers the
    /// order of the checks.
    #[derive(Clone, Default)]
    struct ScriptedChecker {
        script: Arc<Mutex<HashMap<String, VecDeque<CheckResult>>>>,
        checked: Arc<Mutex<Vec<(String, usize)>>>,
    }

    impl ScriptedChecker {
        fn script(self, id: &str, results: Vec<CheckResult>) -> Self {
            self.script
                .lock()
                .unwrap()
                .insert(id.to_string(), results.into());
            self
        }

        fn checked(&self) -> Vec<(String, usize)> {
            self.checked.lock().unwrap().clone()
        }
    }

    impl ComponentChecker for ScriptedChecker {
        async fn check(&self, component_id: &str, rotation: usize) -> CheckResult {
            self.checked
                .lock()
                .unwrap()
                .push((component_id.to_string(), rotation));
            self.script
                .lock()
                .unwrap()
                .get_mut(component_id)
                .and_then(VecDeque::pop_front)
                .unwrap_or(CheckResult::Observed(Observation::BlockUnavailable))
        }
    }

    struct Fixture {
        monitor: Arc<Monitor>,
        stream: Arc<LatestStream>,
    }

    fn fixture(component_ids: &[&str]) -> Fixture {
        let clock = MonitorClock::new();
        let store = Arc::new(HealthStore::new(THRESHOLDS, BTreeMap::new()));
        for id in component_ids {
            store.track(id, "uniswap_v3", clock.now());
        }
        let stream =
            Arc::new(LatestStream::new(Chain::Ethereum, StreamKind::ExcludedComponents, 1));
        let publisher =
            Arc::new(ExcludedPublisher::new(Arc::clone(&stream), Arc::clone(&store), clock));
        let monitor = Arc::new(Monitor {
            chain: Chain::Ethereum,
            store,
            scheduler: ProbeScheduler::new(
                Duration::from_secs(6 * 3600),
                Duration::from_secs(3600),
            ),
            publisher,
            liveness: Arc::new(ProbeLiveness::new(INTERVAL)),
            clock,
        });
        Fixture { monitor, stream }
    }

    fn executed(block: u64, deviation_bps: f64) -> CheckResult {
        CheckResult::Observed(Observation::Executed { block, deviation_bps })
    }

    fn pool_reverted(block: u64) -> CheckResult {
        CheckResult::Observed(Observation::PoolReverted { block, reason: "PoolEmpty".into() })
    }

    fn excluded_ids(stream: &LatestStream) -> Option<Vec<(String, ExclusionCause)>> {
        let message = stream.current()?;
        let decoded: NexusMessage<fynd_core::nexus::components::ExcludedComponents> =
            NexusMessage::decode(std::str::from_utf8(&message.encoded).unwrap()).unwrap();
        let MessageBody::Snapshot { data, .. } = decoded.body else {
            return None;
        };
        Some(
            data.components
                .into_iter()
                .map(|component| (component.id, component.cause))
                .collect(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn test_probes_one_component_per_interval_in_scheduler_order() {
        let fixture = fixture(&["0xa", "0xb", "0xc"]);
        let checker = ScriptedChecker::default()
            .script("0xa", vec![executed(1, 0.0)])
            .script("0xb", vec![executed(1, 0.0)])
            .script("0xc", vec![executed(1, 0.0)]);
        let task = tokio::spawn(
            ComponentProber::new(checker.clone(), Arc::clone(&fixture.monitor), INTERVAL).run(),
        );

        tokio::time::sleep(INTERVAL * 2 + INTERVAL / 2).await;
        let after_three_ticks: Vec<_> = checker
            .checked()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        tokio::time::sleep(INTERVAL * 10).await;
        task.abort();

        assert_eq!(after_three_ticks, ["0xa", "0xb", "0xc"]);
        assert_eq!(
            checker.checked().len(),
            3,
            "components probed cleanly are not due again for hours"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_reverts_on_different_blocks_exclude_the_component_and_recovery_clears_it() {
        let fixture = fixture(&["0xa"]);
        let checker = ScriptedChecker::default()
            .script("0xa", vec![pool_reverted(1), pool_reverted(2), executed(3, 0.0)]);
        let task = tokio::spawn(
            ComponentProber::new(checker.clone(), Arc::clone(&fixture.monitor), INTERVAL).run(),
        );
        assert!(excluded_ids(&fixture.stream).is_none(), "no data before the first probe");

        tokio::time::sleep(INTERVAL).await;
        let after_first = excluded_ids(&fixture.stream);
        tokio::time::sleep(Duration::from_secs(61)).await;
        let after_second = excluded_ids(&fixture.stream);
        tokio::time::sleep(Duration::from_secs(121)).await;
        let after_recovery = excluded_ids(&fixture.stream);
        task.abort();

        assert_eq!(after_first, Some(vec![]));
        assert_eq!(after_second, Some(vec![("0xa".to_string(), ExclusionCause::Broken)]));
        assert_eq!(after_recovery, Some(vec![]));
    }

    #[tokio::test(start_paused = true)]
    async fn test_rotation_counts_the_probes_of_the_component() {
        let fixture = fixture(&["0xa"]);
        let checker = ScriptedChecker::default()
            .script("0xa", vec![pool_reverted(1), pool_reverted(2), pool_reverted(3)]);
        let task = tokio::spawn(
            ComponentProber::new(checker.clone(), Arc::clone(&fixture.monitor), INTERVAL).run(),
        );

        tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        task.abort();

        let rotations: Vec<_> = checker
            .checked()
            .into_iter()
            .map(|(_, rotation)| rotation)
            .take(3)
            .collect();
        assert_eq!(rotations, [0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_component_that_cannot_be_probed_is_marked_and_left_alone() {
        let fixture = fixture(&["0xa"]);
        let checker = ScriptedChecker::default().script("0xa", vec![CheckResult::NotProbeable]);
        let task = tokio::spawn(
            ComponentProber::new(checker.clone(), Arc::clone(&fixture.monitor), INTERVAL).run(),
        );

        tokio::time::sleep(Duration::from_secs(3600)).await;
        task.abort();

        assert_eq!(checker.checked().len(), 1);
        assert_eq!(
            fixture
                .monitor
                .store
                .get("0xa")
                .unwrap()
                .status,
            HealthStatus::NotProbeable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_probes_that_give_no_verdict_change_nothing_and_publish_nothing() {
        let fixture = fixture(&["0xa"]);
        let checker = ScriptedChecker::default().script(
            "0xa",
            vec![
                CheckResult::Observed(Observation::SetupFailed { reason: "rpc down".into() }),
                CheckResult::Observed(Observation::BlockUnavailable),
            ],
        );
        let task = tokio::spawn(
            ComponentProber::new(checker.clone(), Arc::clone(&fixture.monitor), INTERVAL).run(),
        );

        tokio::time::sleep(Duration::from_secs(4 * 60)).await;
        task.abort();

        let health = fixture
            .monitor
            .store
            .get("0xa")
            .unwrap();
        assert_eq!(health.status, HealthStatus::Unknown);
        assert_eq!(health.probes, 3);
        assert!(excluded_ids(&fixture.stream).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_nothing_due_counts_as_idle_and_publishes_once_something_was_probed() {
        let fixture = fixture(&["0xa"]);
        let checker = ScriptedChecker::default().script("0xa", vec![executed(1, 0.0)]);
        let prober = ComponentProber::new(checker, Arc::clone(&fixture.monitor), INTERVAL);

        prober.probe_next().await;
        fixture.stream.withdraw();
        tokio::time::advance(Duration::from_secs(60)).await;
        prober.probe_next().await;

        assert!(fixture.stream.is_serving(), "an idle round publishes after a withdrawal");
        assert_eq!(excluded_ids(&fixture.stream), Some(vec![]));
    }

    #[test]
    fn test_metrics_count_probes_by_protocol_and_outcome() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let _guard = metrics::set_default_local_recorder(&recorder);
        runtime.block_on(async {
            let fixture = fixture(&["0xa", "0xb"]);
            let checker = ScriptedChecker::default()
                .script("0xa", vec![executed(1, -42.0)])
                .script("0xb", vec![pool_reverted(1)]);
            let prober = ComponentProber::new(checker, Arc::clone(&fixture.monitor), INTERVAL);
            prober.probe_next().await;
            prober.probe_next().await;
        });

        let mut counters = Vec::new();
        let mut deviations = Vec::new();
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let labels: Vec<_> = key
                .key()
                .labels()
                .map(|label| format!("{}={}", label.key(), label.value()))
                .collect();
            match value {
                DebugValue::Counter(count) => {
                    counters.push((key.key().name().to_string(), labels, count));
                }
                DebugValue::Histogram(values)
                    if key.key().name() == "nexus_probe_deviation_bps" =>
                {
                    deviations.push((key.key().name().to_string(), labels, values.len()));
                }
                DebugValue::Histogram(_) | DebugValue::Gauge(_) => {}
            }
        }
        counters.sort();
        let labels = |outcome: &str| {
            vec![
                "chain=ethereum".to_string(),
                "protocol=uniswap_v3".to_string(),
                format!("outcome={outcome}"),
            ]
        };
        assert_eq!(counters.len(), 2);
        assert!(counters.contains(&("nexus_probes_total".to_string(), labels("executed"), 1)));
        assert!(counters.contains(&("nexus_probes_total".to_string(), labels("pool_reverted"), 1)));
        assert_eq!(
            deviations,
            [(
                "nexus_probe_deviation_bps".to_string(),
                vec!["chain=ethereum".to_string(), "protocol=uniswap_v3".to_string()],
                1
            )]
        );
    }
}
