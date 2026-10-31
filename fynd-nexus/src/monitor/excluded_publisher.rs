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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use fynd_core::nexus::{MessageBody, NexusMessage};
    use tycho_simulation::evm::tycho_models::Chain;

    use super::*;
    use crate::{
        monitor::component_health::{HealthThresholds, Observation},
        stream_kind::StreamKind,
    };

    const THRESHOLDS: HealthThresholds =
        HealthThresholds { broken_after: 1, inaccurate_after: 3, inaccuracy_bps: 100.0 };

    struct Fixture {
        stream: Arc<LatestStream>,
        store: Arc<HealthStore>,
        publisher: ExcludedPublisher,
    }

    fn fixture() -> Fixture {
        let stream =
            Arc::new(LatestStream::new(Chain::Ethereum, StreamKind::ExcludedComponents, 1));
        let store = Arc::new(HealthStore::new(THRESHOLDS, Default::default()));
        let publisher =
            ExcludedPublisher::new(Arc::clone(&stream), Arc::clone(&store), MonitorClock::new());
        Fixture { stream, store, publisher }
    }

    fn seq(stream: &LatestStream) -> u64 {
        stream
            .current()
            .map_or(0, |message| message.seq)
    }

    fn revert(reason: &str, block: u64) -> Observation {
        Observation::PoolReverted { block, reason: reason.to_string() }
    }

    #[test]
    fn test_first_sync_publishes_even_an_empty_set() {
        let fixture = fixture();

        fixture.publisher.sync();

        let message = fixture.stream.current().unwrap();
        let decoded: NexusMessage<ExcludedComponents> =
            NexusMessage::decode(std::str::from_utf8(&message.encoded).unwrap()).unwrap();
        let MessageBody::Snapshot { data, .. } = decoded.body else {
            panic!("expected a snapshot");
        };
        assert!(data.components.is_empty());
    }

    #[test]
    fn test_publishes_only_when_the_set_or_a_cause_changes() {
        let fixture = fixture();
        let now = Utc::now();
        fixture
            .store
            .track("0xa", "uniswap_v3", now);
        fixture.publisher.sync();
        let after_empty = seq(&fixture.stream);

        fixture.publisher.sync();
        let unchanged = seq(&fixture.stream);
        fixture
            .store
            .record("0xa", revert("PoolEmpty", 1), now);
        fixture.publisher.sync();
        let after_broken = seq(&fixture.stream);
        fixture
            .store
            .record("0xa", revert("OtherError", 2), now);
        fixture.publisher.sync();
        let after_new_reason = seq(&fixture.stream);
        fixture.store.reset_one("0xa", now);
        fixture.publisher.sync();
        let after_reset = seq(&fixture.stream);

        assert_eq!(
            (after_empty, unchanged, after_broken, after_new_reason, after_reset),
            (1, 1, 2, 2, 3)
        );
    }

    #[test]
    fn test_publishes_again_after_a_withdrawal() {
        let fixture = fixture();
        fixture.publisher.sync();

        fixture.publisher.withdraw();
        assert!(!fixture.stream.is_serving());
        fixture.publisher.sync();

        assert!(fixture.stream.is_serving());
        assert_eq!(seq(&fixture.stream), 3);
    }
}
