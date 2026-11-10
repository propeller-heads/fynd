//! Writes the market's default derived computations into the derived snapshot store.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use fynd_core::{
    derived::{DerivedDataEvent, SharedDerivedDataRef},
    nexus::derived::{
        DerivedKind, DerivedSnapshot, PoolDepthSnapshot, SpotPriceSnapshot, TokenGasPriceSnapshot,
    },
};
use metrics::counter;
use serde::Serialize;
use tokio::{sync::broadcast, task::JoinHandle};
use tracing::{debug, error, warn};

use crate::derived::snapshot::DerivedSnapshots;

/// Writes the latest snapshot of each derived computation into `DerivedSnapshots` when the market
/// computes a new block.
pub struct DerivedSnapshotWriter {
    derived_data: SharedDerivedDataRef,
    snapshots: Arc<DerivedSnapshots>,
}

impl DerivedSnapshotWriter {
    /// A writer of `derived_data` into `snapshots`.
    pub fn new(derived_data: SharedDerivedDataRef, snapshots: Arc<DerivedSnapshots>) -> Self {
        Self { derived_data, snapshots }
    }

    /// Writes what the store already holds, then follows `events`. The task ends when the
    /// market's event channel closes.
    pub fn spawn(self, events: broadcast::Receiver<DerivedDataEvent>) -> JoinHandle<()> {
        tokio::spawn(Arc::new(self).run(events))
    }

    async fn run(self: Arc<Self>, mut events: broadcast::Receiver<DerivedDataEvent>) {
        Arc::clone(&self)
            .write_off_runtime(DerivedKind::ALL.to_vec())
            .await;
        loop {
            match events.recv().await {
                Ok(DerivedDataEvent::ComputationComplete { computation_id, .. }) => {
                    if let Some(kind) = DerivedKind::from_computation_id(computation_id) {
                        Arc::clone(&self)
                            .write_off_runtime(vec![kind])
                            .await;
                    }
                }
                Ok(
                    DerivedDataEvent::NewBlock { .. } | DerivedDataEvent::ComputationFailed { .. },
                ) => {}
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "missed derived data events, writing every kind again");
                    Arc::clone(&self)
                        .write_off_runtime(DerivedKind::ALL.to_vec())
                        .await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Uses the blocking pool because converting and encoding a large market's maps takes
    /// milliseconds and would stall the runtime's other tasks.
    async fn write_off_runtime(self: Arc<Self>, kinds: Vec<DerivedKind>) {
        let writing = tokio::task::spawn_blocking(move || {
            for kind in kinds {
                self.write(kind);
            }
        });
        if let Err(join_error) = writing.await {
            error!(%join_error, "writing derived data failed");
        }
    }

    /// Writes the current snapshot for `kind` if the market has computed that data and has not
    /// written its block yet.
    fn write(&self, kind: DerivedKind) {
        let derived_data = self.derived_data.blocking_read();
        let started = Instant::now();
        match kind {
            DerivedKind::SpotPrices => {
                let Some((prices, status)) = derived_data.spot_prices_with_status() else {
                    return;
                };
                if self.is_written(kind, status.block()) {
                    return;
                }
                let snapshot = SpotPriceSnapshot::from_spot_prices(prices, status.block());
                let skipped = prices.len() - snapshot.entries.len();
                drop(derived_data);
                if skipped > 0 {
                    warn!(
                        %kind,
                        block = snapshot.block,
                        skipped,
                        "left out prices that are not finite"
                    );
                    counter!("nexus_derived_skipped_entries_total", self.snapshots.labels(kind))
                        .increment(skipped as u64);
                }
                self.write_snapshot(kind, &snapshot, started);
            }
            DerivedKind::PoolDepths => {
                let Some((depths, status)) = derived_data.component_depths_with_status() else {
                    return;
                };
                if self.is_written(kind, status.block()) {
                    return;
                }
                let snapshot = PoolDepthSnapshot::from_depths(depths, status.block());
                drop(derived_data);
                self.write_snapshot(kind, &snapshot, started);
            }
            DerivedKind::TokenPrices => {
                let Some((prices, status)) = derived_data.token_prices_with_status() else {
                    return;
                };
                if self.is_written(kind, status.block()) {
                    return;
                }
                let snapshot = TokenGasPriceSnapshot::from_token_prices(prices, status.block());
                drop(derived_data);
                self.write_snapshot(kind, &snapshot, started);
            }
        }
    }

    fn is_written(&self, kind: DerivedKind, block: u64) -> bool {
        self.snapshots.block(kind) == Some(block)
    }

    fn write_snapshot<E: Serialize>(
        &self,
        kind: DerivedKind,
        snapshot: &DerivedSnapshot<E>,
        started: Instant,
    ) {
        let converted = started.elapsed();
        let writing = Instant::now();
        self.snapshots.replace(kind, snapshot);
        debug!(
            %kind,
            block = snapshot.block,
            entries = snapshot.entries.len(),
            convert_ms = millis(converted),
            write_ms = millis(writing.elapsed()),
            "wrote derived snapshot"
        );
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}
