//! The components a `fynd-nexus` service found unhealthy, and the poller that applies them in a
//! pod.
//!
//! The service probes each Tycho component on chain and serves the ones pods should not route
//! through, inside a [`NexusMessage`](super::NexusMessage). The message carries
//! [`ExcludedComponents`] as its snapshot data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ComponentId;

/// The path that serves the excluded components of a `fynd-nexus` service.
pub fn excluded_path() -> &'static str {
    "/v1/components/excluded"
}

/// Why a component is excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionCause {
    /// Swaps through the component revert on chain, for a reason that is not a token's.
    Broken,
    /// Swaps execute, but pay less than the component's state predicted.
    Inaccurate,
}

impl ExclusionCause {
    /// The name of the cause, as in the JSON and the `cause` metric label.
    pub fn name(self) -> &'static str {
        match self {
            Self::Broken => "broken",
            Self::Inaccurate => "inaccurate",
        }
    }
}

/// One component that pods should not route through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedComponent {
    /// The component id, as Tycho emits it: a lowercase `0x` hex string.
    pub id: ComponentId,
    /// The protocol system of the component.
    pub protocol_system: String,
    /// Why the component is excluded.
    pub cause: ExclusionCause,
    /// When the component became excluded.
    pub since: DateTime<Utc>,
    /// The last outcome that made the component unhealthy, for people to read.
    pub reason: String,
}

/// Every component pods should not route through. A component not listed is routable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedComponents {
    /// The excluded components.
    pub components: Vec<ExcludedComponent>,
}
