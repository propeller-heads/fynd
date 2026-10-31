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
