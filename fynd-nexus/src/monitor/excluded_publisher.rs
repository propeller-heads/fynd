//! Publishes the components pods should not route through to the excluded components stream.

use std::sync::{Arc, Mutex, PoisonError};

use fynd_core::{
    nexus::components::{ExcludedComponents, ExclusionCause},
    ComponentId,
};

use crate::{
    latest_stream::LatestStream,
    monitor::{health_store::HealthStore, monitor_clock::MonitorClock},
};

/// Who is excluded and why, without the details that change from probe to probe.
type ExcludedSet = Vec<(ComponentId, ExclusionCause)>;

/// Keeps the excluded components stream in step with the health store.
///
/// It publishes a new snapshot only when the set of excluded components or the cause of one
/// changes, so that pods see a new message only then. It publishes again after a withdrawal.
pub struct ExcludedPublisher {
    stream: Arc<LatestStream>,
    store: Arc<HealthStore>,
    clock: MonitorClock,
    published: Mutex<Option<ExcludedSet>>,
}

impl ExcludedPublisher {
    /// A publisher of the excluded components of `store` to `stream`.
    pub fn new(stream: Arc<LatestStream>, store: Arc<HealthStore>, clock: MonitorClock) -> Self {
        Self { stream, store, clock, published: Mutex::new(None) }
    }

    /// Publishes the excluded components if they differ from the ones the stream serves, or if
    /// the stream does not serve a snapshot.
    pub fn sync(&self) {
        let components = self.store.excluded();
        let set: ExcludedSet = components
            .iter()
            .map(|component| (component.id.clone(), component.cause))
            .collect();
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.stream.is_serving() && published.as_ref() == Some(&set) {
            return;
        }
        self.stream
            .publish(&ExcludedComponents { components }, self.clock.now());
        *published = Some(set);
    }

    /// Withdraws the stream, so that pods exclude nothing.
    pub fn withdraw(&self) {
        self.stream.withdraw();
    }
}
