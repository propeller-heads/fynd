//! Reports how many components are in each status as `nexus_components`.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use metrics::gauge;

use crate::monitor::{component_health::HealthStatus, health_store::HealthStore};

/// How often the gauges are refreshed.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Sets `nexus_components{protocol,status}` from `store` every few seconds.
///
/// Every status of every protocol the store has held is set on every round, so that a status
/// that empties reads zero instead of keeping its last value.
pub async fn report_components(store: Arc<HealthStore>, chain: String) {
    let mut ticker = tokio::time::interval(REPORT_INTERVAL);
    let mut protocols = BTreeSet::new();
    loop {
        ticker.tick().await;
        report_once(&store, &chain, &mut protocols);
    }
}

/// Sets the gauges once. `protocols` collects the protocol systems seen so far.
fn report_once(store: &HealthStore, chain: &str, protocols: &mut BTreeSet<String>) {
    let counts = store.status_counts();
    protocols.extend(
        counts
            .keys()
            .map(|(protocol, _)| protocol.clone()),
    );
    for protocol in protocols.iter() {
        for status in HealthStatus::ALL {
            let count = counts
                .get(&(protocol.clone(), status))
                .copied()
                .unwrap_or(0);
            gauge!(
                "nexus_components",
                "chain" => chain.to_string(),
                "protocol" => protocol.clone(),
                "status" => status.name()
            )
            .set(count as f64);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

    use super::*;
    use crate::monitor::component_health::{HealthThresholds, Observation};

    fn gauge_values(snapshotter: &Snapshotter) -> Vec<(String, String, f64)> {
        let mut values = Vec::new();
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let DebugValue::Gauge(count) = value else {
                continue;
            };
            let label = |name: &str| {
                key.key()
                    .labels()
                    .find(|label| label.key() == name)
                    .map(|label| label.value().to_string())
                    .unwrap_or_default()
            };
            values.push((label("protocol"), label("status"), count.into_inner()));
        }
        values.sort_by(|left, right| left.partial_cmp(right).unwrap());
        values
    }

    #[test]
    fn test_counts_per_protocol_and_status_and_zeroes_emptied_ones() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let store = HealthStore::new(
            HealthThresholds { broken_after: 1, inaccurate_after: 3, inaccuracy_bps: 100.0 },
            Default::default(),
        );
        let now = Utc::now();
        store.track("0xa", "uniswap_v3", now);
        store.track("0xb", "uniswap_v3", now);
        store.track("0xc", "uniswap_v2", now);
        let mut protocols = BTreeSet::new();

        metrics::with_local_recorder(&recorder, || {
            store.record(
                "0xa",
                Observation::PoolReverted { block: 1, reason: "x".to_string() },
                now,
            );
            report_once(&store, "ethereum", &mut protocols);
        });
        let first = gauge_values(&snapshotter);
        metrics::with_local_recorder(&recorder, || {
            store.untrack("0xc");
            report_once(&store, "ethereum", &mut protocols);
        });
        let second = gauge_values(&snapshotter);

        let count = |values: &[(String, String, f64)], protocol: &str, status: &str| {
            values
                .iter()
                .find(|(p, s, _)| p == protocol && s == status)
                .map(|(_, _, count)| *count)
        };
        assert_eq!(first.len(), 12, "six statuses of two protocols");
        assert_eq!(count(&first, "uniswap_v3", "broken"), Some(1.0));
        assert_eq!(count(&first, "uniswap_v3", "unknown"), Some(1.0));
        assert_eq!(count(&first, "uniswap_v2", "unknown"), Some(1.0));
        assert_eq!(count(&first, "uniswap_v2", "healthy"), Some(0.0));
        assert_eq!(count(&second, "uniswap_v2", "unknown"), Some(0.0));
    }
}
