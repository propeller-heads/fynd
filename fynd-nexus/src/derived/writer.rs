//! Publishes the market's default derived computations into the derived snapshot store.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use fynd_core::{
    derived::{DerivedDataEvent, SharedDerivedDataRef},
    nexus::derived::DerivedKind,
};
use metrics::counter;
use tokio::{sync::broadcast, task::JoinHandle};
use tracing::{debug, error, warn};

use crate::derived::{history::PublishSummary, snapshot::DerivedSnapshots};

/// Publishes each derived computation into `DerivedSnapshots` when the market computes a new
/// block.
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

    /// Uses the blocking pool because diffing a large market's maps takes
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

    /// Publishes the current output of `kind` if the market has computed it and it differs from
    /// what was published, or comes from another block.
    ///
    /// The diff is taken against the last published output rather than from the market's change
    /// lists, because the writer can skip blocks when it lags. It holds the derived data read
    /// lock for the diff only.
    fn write(&self, kind: DerivedKind) {
        let derived_data = self.derived_data.blocking_read();
        let started = Instant::now();
        let summary = match kind {
            DerivedKind::SpotPrices => {
                let Some((prices, status)) = derived_data.spot_prices_with_status() else {
                    return;
                };
                self.snapshots
                    .spot_prices()
                    .publish(prices, status.block())
            }
            DerivedKind::PoolDepths => {
                let Some((depths, status)) = derived_data.component_depths_with_status() else {
                    return;
                };
                self.snapshots
                    .pool_depths()
                    .publish(depths, status.block())
            }
            DerivedKind::TokenPrices => {
                let Some((prices, status)) = derived_data.token_prices_with_status() else {
                    return;
                };
                self.snapshots
                    .token_prices()
                    .publish(prices, status.block())
            }
        };
        drop(derived_data);
        if let Some(summary) = summary {
            self.report(kind, summary, started);
        }
    }

    fn report(&self, kind: DerivedKind, summary: PublishSummary, started: Instant) {
        if summary.non_finite_left_out > 0 {
            warn!(
                %kind,
                block = summary.block,
                skipped = summary.non_finite_left_out,
                "left out prices that are not finite"
            );
            counter!("nexus_derived_skipped_entries_total", self.snapshots.labels(kind))
                .increment(summary.non_finite_left_out as u64);
        }
        debug!(
            %kind,
            seq = summary.seq,
            block = summary.block,
            upserted = summary.upserted,
            removed = summary.removed,
            publish_ms = millis(started.elapsed()),
            "published derived data"
        );
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}
