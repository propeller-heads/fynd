//! The health monitor: probes every Tycho component of the chain on chain and serves the ones
//! pods should not route through.
//!
//! [`component_health`] holds one component's status and the rules that move it, and
//! [`health_store`] holds the records of all components. [`health_file`] keeps them across
//! restarts. [`probe_scheduler`] picks the component to probe next, [`component_checker`] plans
//! and runs the probe, and [`component_prober`] loops over both at the probe rate.
//! [`excluded_publisher`] publishes the broken and inaccurate components to the excluded
//! components stream, and [`probe_liveness`] withdraws it when probing stalls.

pub mod component_checker;
pub mod component_health;
pub mod component_prober;
pub mod excluded_publisher;
pub mod health_file;
pub mod health_store;
pub mod monitor_args;
pub mod monitor_clock;
pub mod probe_liveness;
pub mod probe_scheduler;

use std::sync::Arc;

use tycho_simulation::evm::tycho_models::Chain;

use crate::monitor::{
    excluded_publisher::ExcludedPublisher, health_store::HealthStore, monitor_clock::MonitorClock,
    probe_liveness::ProbeLiveness, probe_scheduler::ProbeScheduler,
};

/// The parts of the health monitor that the prober and the API share.
pub struct Monitor {
    /// The chain the monitor serves.
    pub chain: Chain,
    /// The health of every tracked component.
    pub store: Arc<HealthStore>,
    /// Picks the component to probe next.
    pub scheduler: ProbeScheduler,
    /// Publishes the excluded components.
    pub publisher: Arc<ExcludedPublisher>,
    /// Tells whether probing has stalled.
    pub liveness: Arc<ProbeLiveness>,
    /// The date, driven by Tokio's clock.
    pub clock: MonitorClock,
}

impl Monitor {
    /// Resets every component to unknown at the front of the probe queue, publishes the new
    /// excluded set, and returns how many components were reset.
    pub fn reset_all(&self) -> usize {
        let count = self.store.reset_all(self.clock.now());
        self.publisher.sync();
        count
    }

    /// Resets one component the same way. Returns whether the component is tracked.
    pub fn reset_one(&self, component_id: &str) -> bool {
        let known = self
            .store
            .reset_one(component_id, self.clock.now());
        if known {
            self.publisher.sync();
        }
        known
    }
}
