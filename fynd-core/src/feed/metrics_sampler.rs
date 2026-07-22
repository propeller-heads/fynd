//! Background sampler exporting per-protocol market metrics.
//!
//! Runs off the block-update path: on a fixed interval it takes a non-blocking
//! read of [`MarketState`](crate::feed::market_data::MarketState) and exports
//! pool counts and synchronizer status per protocol. Sampling independently of
//! the Tycho feed keeps these gauges flowing even when the feed itself is
//! stalled — which is exactly when the sync-status metrics matter.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use metrics::gauge;
use tokio::time::{interval, MissedTickBehavior};
use tycho_simulation::tycho_client::feed::SynchronizerState;

use crate::feed::market_data::MarketData;

/// Periodically exports per-protocol pool counts and sync status as gauges.
pub(crate) struct MetricsSampler {
    market_data: MarketData,
    sample_interval: Duration,
}

impl MetricsSampler {
    pub(crate) fn new(market_data: MarketData, sample_interval: Duration) -> Self {
        Self { market_data, sample_interval }
    }

    pub(crate) async fn run(&self) {
        let mut ticker = interval(self.sample_interval);
        // Skip missed ticks rather than catching up — samples are best-effort.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            // Non-blocking read: if a block update holds the write lock, skip
            // this tick instead of contending with the feed.
            let Some(state) = self.market_data.try_read() else {
                continue;
            };
            let pool_counts = state.pool_counts_by_protocol().clone();
            let sync_states = state.protocol_sync_states().clone();
            drop(state);

            emit_market_metrics(&pool_counts, &sync_states);
        }
    }
}

/// Sets the per-protocol gauges from a snapshot of pool counts and sync states.
///
/// Protocols are the union of both maps: a protocol that reported a sync
/// status but has no pools yet exports a zero pool count rather than no series.
fn emit_market_metrics(
    pool_counts: &HashMap<String, u64>,
    sync_states: &HashMap<String, SynchronizerState>,
) {
    let protocols: HashSet<&String> = pool_counts
        .keys()
        .chain(sync_states.keys())
        .collect();
    for protocol in protocols {
        let count = pool_counts
            .get(protocol)
            .copied()
            .unwrap_or(0);
        gauge!("market_pools_per_protocol", "protocol" => protocol.clone()).set(count as f64);
    }

    for (protocol, sync_state) in sync_states {
        gauge!("market_protocol_sync_state", "protocol" => protocol.clone())
            .set(sync_state_code(sync_state));
        if let Some(block_number) = sync_state_block_number(sync_state) {
            gauge!("market_protocol_block", "protocol" => protocol.clone())
                .set(block_number as f64);
        }
    }
}

/// Numeric code per synchronizer state, matching the value mappings the
/// existing `tycho_integration_protocol_sync_state` Grafana panels use, so
/// both metrics can share dashboard and alert definitions.
fn sync_state_code(sync_state: &SynchronizerState) -> f64 {
    match sync_state {
        SynchronizerState::Started => 1.0,
        SynchronizerState::Ready(_) => 2.0,
        SynchronizerState::Delayed(_) => 3.0,
        SynchronizerState::Stale(_) => 4.0,
        SynchronizerState::Advanced(_) => 5.0,
        SynchronizerState::Ended(_) => 6.0,
    }
}

fn sync_state_block_number(sync_state: &SynchronizerState) -> Option<u64> {
    match sync_state {
        SynchronizerState::Started | SynchronizerState::Ended(_) => None,
        SynchronizerState::Ready(header) |
        SynchronizerState::Delayed(header) |
        SynchronizerState::Stale(header) |
        SynchronizerState::Advanced(header) => Some(header.number),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use tycho_simulation::tycho_client::feed::{BlockHeader, SynchronizerState};

    use super::*;

    fn header(number: u64) -> BlockHeader {
        BlockHeader { number, ..Default::default() }
    }

    /// Returns the gauge recorded for `name` carrying every label in `labels`.
    fn find_gauge(
        recorded: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
    ) -> f64 {
        let value = recorded
            .iter()
            .find(|(key, _, _, _)| {
                key.key().name() == name &&
                    labels
                        .iter()
                        .all(|(label_key, label_value)| {
                            key.key()
                                .labels()
                                .any(|l| l.key() == *label_key && l.value() == *label_value)
                        })
            })
            .map(|(_, _, _, value)| value)
            .unwrap_or_else(|| panic!("missing {name}{labels:?}, got {recorded:?}"));
        match value {
            DebugValue::Gauge(v) => v.0,
            other => panic!("expected gauge for {name}{labels:?}, got {other:?}"),
        }
    }

    #[test]
    fn emit_market_metrics_records_pool_and_sync_gauges() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        let pool_counts =
            HashMap::from([("uniswap_v2".to_string(), 100u64), ("curve".to_string(), 5u64)]);
        let sync_states = HashMap::from([
            ("uniswap_v2".to_string(), SynchronizerState::Ready(header(42))),
            ("uniswap_v3".to_string(), SynchronizerState::Stale(header(30))),
        ]);

        metrics::with_local_recorder(&recorder, || {
            emit_market_metrics(&pool_counts, &sync_states);
        });

        let recorded = snapshotter.snapshot().into_vec();

        assert_eq!(
            find_gauge(&recorded, "market_pools_per_protocol", &[("protocol", "uniswap_v2")]),
            100.0
        );
        // A protocol with pools but no sync status yet is still exported.
        assert_eq!(
            find_gauge(&recorded, "market_pools_per_protocol", &[("protocol", "curve")]),
            5.0
        );
        // A protocol with a sync status but no pools yet must export zero, not
        // be absent, so dashboards see it immediately.
        assert_eq!(
            find_gauge(&recorded, "market_pools_per_protocol", &[("protocol", "uniswap_v3")]),
            0.0
        );

        // Sync state is a numeric code (1=started .. 6=ended), matching the
        // value mappings used by the tycho_integration_protocol_sync_state
        // Grafana panels: ready = 2, stale = 4.
        assert_eq!(
            find_gauge(&recorded, "market_protocol_sync_state", &[("protocol", "uniswap_v2")]),
            2.0
        );
        assert_eq!(
            find_gauge(&recorded, "market_protocol_sync_state", &[("protocol", "uniswap_v3")]),
            4.0
        );

        assert_eq!(
            find_gauge(&recorded, "market_protocol_block", &[("protocol", "uniswap_v2")]),
            42.0
        );
        assert_eq!(
            find_gauge(&recorded, "market_protocol_block", &[("protocol", "uniswap_v3")]),
            30.0
        );
    }
}
