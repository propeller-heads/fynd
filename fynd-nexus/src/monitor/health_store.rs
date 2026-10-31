//! The health records of every component the monitor tracks, in memory.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, PoisonError},
};

use chrono::{DateTime, Utc};
use fynd_core::{
    nexus::components::{ExcludedComponent, ExclusionCause},
    ComponentId,
};

use crate::monitor::component_health::{
    ComponentHealth, HealthStatus, HealthThresholds, Observation,
};

/// The records by component id, and whether they changed since they were last saved.
struct Records {
    components: BTreeMap<ComponentId, ComponentHealth>,
    unsaved: bool,
}

/// The health records of the tracked components, shared by the prober and the API.
///
/// Every change to a record marks the store as unsaved, so that the file writer knows when to
/// write.
pub struct HealthStore {
    thresholds: HealthThresholds,
    records: Mutex<Records>,
}

impl HealthStore {
    /// A store that holds `components`, which it applies `thresholds` to.
    pub fn new(
        thresholds: HealthThresholds,
        components: BTreeMap<ComponentId, ComponentHealth>,
    ) -> Self {
        Self { thresholds, records: Mutex::new(Records { components, unsaved: false }) }
    }

    /// Starts tracking a component of `protocol_system` at the front of the probe queue, unless
    /// it is tracked already.
    pub fn track(&self, id: &str, protocol_system: &str, now: DateTime<Utc>) {
        let mut records = self.lock();
        if !records.components.contains_key(id) {
            records
                .components
                .insert(id.to_string(), ComponentHealth::new(protocol_system.to_string(), now));
            records.unsaved = true;
        }
    }

    /// Stops tracking a component and forgets its health.
    pub fn untrack(&self, id: &str) {
        let mut records = self.lock();
        if records.components.remove(id).is_some() {
            records.unsaved = true;
        }
    }

    /// Stops tracking every component that `keep` rejects.
    pub fn retain(&self, mut keep: impl FnMut(&str) -> bool) {
        let mut records = self.lock();
        let before = records.components.len();
        records
            .components
            .retain(|id, _| keep(id));
        if records.components.len() != before {
            records.unsaved = true;
        }
    }

    /// Applies a probe of the component made at `now`. Does nothing for an untracked component.
    pub fn record(&self, id: &str, observation: Observation, now: DateTime<Utc>) {
        let mut records = self.lock();
        if let Some(health) = records.components.get_mut(id) {
            health.record(observation, &self.thresholds, now);
            records.unsaved = true;
        }
    }

    /// Marks a component as one that cannot be probed.
    pub fn mark_not_probeable(&self, id: &str, now: DateTime<Utc>) {
        let mut records = self.lock();
        if let Some(health) = records.components.get_mut(id) {
            health.mark_not_probeable(now);
            records.unsaved = true;
        }
    }

    /// Resets every component to unknown at the front of the probe queue, and returns how many
    /// there are.
    pub fn reset_all(&self, now: DateTime<Utc>) -> usize {
        let mut records = self.lock();
        for health in records.components.values_mut() {
            health.reset(now);
        }
        records.unsaved = true;
        records.components.len()
    }

    /// Resets one component to unknown at the front of the probe queue. Returns whether the
    /// component is tracked.
    pub fn reset_one(&self, id: &str, now: DateTime<Utc>) -> bool {
        let mut records = self.lock();
        let Some(health) = records.components.get_mut(id) else {
            return false;
        };
        health.reset(now);
        records.unsaved = true;
        true
    }

    /// The record of one component.
    pub fn get(&self, id: &str) -> Option<ComponentHealth> {
        self.lock().components.get(id).cloned()
    }

    /// The records of the components with `status` and `protocol_system`, where given, by id.
    pub fn report(
        &self,
        status: Option<HealthStatus>,
        protocol_system: Option<&str>,
    ) -> Vec<(ComponentId, ComponentHealth)> {
        self.lock()
            .components
            .iter()
            .filter(|(_, health)| status.is_none_or(|wanted| health.status == wanted))
            .filter(|(_, health)| {
                protocol_system.is_none_or(|wanted| health.protocol_system == wanted)
            })
            .map(|(id, health)| (id.clone(), health.clone()))
            .collect()
    }

    /// Reads the records without copying them.
    pub fn with_components<R>(
        &self,
        read: impl FnOnce(&BTreeMap<ComponentId, ComponentHealth>) -> R,
    ) -> R {
        read(&self.lock().components)
    }

    /// The components pods should not route through, by id.
    pub fn excluded(&self) -> Vec<ExcludedComponent> {
        let mut excluded = Vec::new();
        for (id, health) in &self.lock().components {
            let cause = match health.status {
                HealthStatus::Broken => ExclusionCause::Broken,
                HealthStatus::Inaccurate => ExclusionCause::Inaccurate,
                HealthStatus::Unknown |
                HealthStatus::Healthy |
                HealthStatus::Suspect |
                HealthStatus::NotProbeable => continue,
            };
            excluded.push(ExcludedComponent {
                id: id.clone(),
                protocol_system: health.protocol_system.clone(),
                cause,
                since: health.status_since,
                reason: health
                    .reason
                    .clone()
                    .unwrap_or_default(),
            });
        }
        excluded
    }

    /// How many components have each status, by protocol system.
    pub fn status_counts(&self) -> BTreeMap<(String, HealthStatus), usize> {
        let mut counts = BTreeMap::new();
        for health in self.lock().components.values() {
            *counts
                .entry((health.protocol_system.clone(), health.status))
                .or_insert(0) += 1;
        }
        counts
    }

    /// Whether any tracked component has a verdict from a probe.
    pub fn has_verdicts(&self) -> bool {
        self.lock()
            .components
            .values()
            .any(|health| health.last_outcome.is_some())
    }

    /// A copy of every record if any changed since the last call, and marks the store as saved.
    pub fn take_unsaved(&self) -> Option<BTreeMap<ComponentId, ComponentHealth>> {
        let mut records = self.lock();
        if !records.unsaved {
            return None;
        }
        records.unsaved = false;
        Some(records.components.clone())
    }

    /// Marks the store as unsaved again, after a write failed.
    pub fn mark_unsaved(&self) {
        self.lock().unsaved = true;
    }

    fn lock(&self) -> MutexGuard<'_, Records> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::component_health::LastOutcome;

    const THRESHOLDS: HealthThresholds =
        HealthThresholds { broken_after: 2, inaccurate_after: 3, inaccuracy_bps: 100.0 };

    fn store() -> HealthStore {
        HealthStore::new(THRESHOLDS, BTreeMap::new())
    }

    fn revert(block: u64) -> Observation {
        Observation::PoolReverted { block, reason: "PoolEmpty".to_string() }
    }

    fn break_component(store: &HealthStore, id: &str, now: DateTime<Utc>) {
        store.record(id, revert(1), now);
        store.record(id, revert(2), now);
    }

    #[test]
    fn test_excluded_lists_broken_and_inaccurate_components_by_id() {
        let store = store();
        let now = Utc::now();
        for id in ["0xc", "0xb", "0xa", "0xd"] {
            store.track(id, "uniswap_v3", now);
        }
        break_component(&store, "0xc", now);
        for block in 1..=3 {
            store.record("0xa", Observation::Executed { block, deviation_bps: -250.0 }, now);
        }
        store.record("0xb", revert(1), now);
        store.record("0xd", Observation::Executed { block: 1, deviation_bps: 0.0 }, now);

        let excluded = store.excluded();

        let summary: Vec<_> = excluded
            .iter()
            .map(|component| (component.id.as_str(), component.cause))
            .collect();
        assert_eq!(summary, [("0xa", ExclusionCause::Inaccurate), ("0xc", ExclusionCause::Broken)]);
        assert_eq!(excluded[0].reason, "pays 250 bps less than predicted");
        assert_eq!(excluded[1].reason, "PoolEmpty");
        assert_eq!(excluded[1].since, now);
    }

    #[test]
    fn test_track_keeps_the_record_of_a_tracked_component() {
        let store = store();
        let now = Utc::now();
        store.track("0xa", "uniswap_v3", now);
        store.record("0xa", revert(1), now);

        store.track("0xa", "uniswap_v3", now + chrono::Duration::seconds(5));

        assert_eq!(store.get("0xa").unwrap().last_outcome, Some(LastOutcome::PoolReverted));
    }

    #[test]
    fn test_untrack_and_retain_forget_components() {
        let store = store();
        let now = Utc::now();
        for id in ["0xa", "0xb", "0xc"] {
            store.track(id, "uniswap_v3", now);
        }

        store.untrack("0xa");
        store.retain(|id| id != "0xb");

        let ids: Vec<_> = store
            .report(None, None)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["0xc"]);
    }

    #[test]
    fn test_record_of_an_untracked_component_is_ignored() {
        let store = store();

        store.record("0xa", revert(1), Utc::now());

        assert!(store.get("0xa").is_none());
        assert!(store.take_unsaved().is_none());
    }

    #[test]
    fn test_reset_one_moves_a_component_to_the_front_and_clears_it() {
        let store = store();
        let now = Utc::now();
        let later = now + chrono::Duration::seconds(30);
        store.track("0xa", "uniswap_v3", now);
        store.track("0xb", "uniswap_v3", now);
        break_component(&store, "0xa", now);

        let reset_known = store.reset_one("0xa", later);
        let reset_unknown = store.reset_one("0xz", later);

        let health = store.get("0xa").unwrap();
        assert!(reset_known && !reset_unknown);
        assert_eq!(
            (health.status, health.queued_at, health.checked_at),
            (HealthStatus::Unknown, later, None)
        );
        assert!(store.excluded().is_empty());
        assert_eq!(store.get("0xb").unwrap().queued_at, now);
    }

    #[test]
    fn test_reset_all_clears_every_component() {
        let store = store();
        let now = Utc::now();
        for id in ["0xa", "0xb"] {
            store.track(id, "uniswap_v3", now);
            break_component(&store, id, now);
        }

        let count = store.reset_all(now);

        assert_eq!(count, 2);
        assert!(store.excluded().is_empty());
        assert!(!store.has_verdicts());
    }

    #[test]
    fn test_report_filters_by_status_and_protocol() {
        let store = store();
        let now = Utc::now();
        store.track("0xa", "uniswap_v3", now);
        store.track("0xb", "uniswap_v2", now);
        store.track("0xc", "uniswap_v2", now);
        store.record("0xb", Observation::Executed { block: 1, deviation_bps: 0.0 }, now);
        let ids = |status, protocol| -> Vec<String> {
            store
                .report(status, protocol)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };

        assert_eq!(ids(None, None), ["0xa", "0xb", "0xc"]);
        assert_eq!(ids(Some(HealthStatus::Healthy), None), ["0xb"]);
        assert_eq!(ids(None, Some("uniswap_v2")), ["0xb", "0xc"]);
        assert_eq!(ids(Some(HealthStatus::Unknown), Some("uniswap_v2")), ["0xc"]);
        assert!(ids(Some(HealthStatus::Broken), None).is_empty());
    }

    #[test]
    fn test_status_counts_per_protocol() {
        let store = store();
        let now = Utc::now();
        store.track("0xa", "uniswap_v3", now);
        store.track("0xb", "uniswap_v3", now);
        store.track("0xc", "uniswap_v2", now);
        store.record("0xb", Observation::Executed { block: 1, deviation_bps: 0.0 }, now);

        let counts = store.status_counts();

        assert_eq!(counts[&("uniswap_v3".to_string(), HealthStatus::Unknown)], 1);
        assert_eq!(counts[&("uniswap_v3".to_string(), HealthStatus::Healthy)], 1);
        assert_eq!(counts[&("uniswap_v2".to_string(), HealthStatus::Unknown)], 1);
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn test_take_unsaved_hands_out_once_per_change() {
        let store = store();
        assert!(store.take_unsaved().is_none());
        store.track("0xa", "uniswap_v3", Utc::now());

        let first = store.take_unsaved();
        let second = store.take_unsaved();
        store.mark_unsaved();

        assert_eq!(first.map(|records| records.len()), Some(1));
        assert!(second.is_none());
        assert!(store.take_unsaved().is_some());
    }
}
