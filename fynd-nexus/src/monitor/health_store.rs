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

    /// Whether any tracked component has been probed.
    pub fn has_probed_components(&self) -> bool {
        self.lock()
            .components
            .values()
            .any(|health| health.checked_at.is_some())
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
