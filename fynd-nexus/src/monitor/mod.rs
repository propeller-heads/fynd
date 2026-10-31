//! The health monitor: probes every Tycho component of the chain on chain and serves the ones
//! pods should not route through.
//!
//! [`component_health`] holds one component's status and the rules that move it.
pub mod component_health;
