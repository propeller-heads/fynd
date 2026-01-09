//! API request and response types.

use serde::{Deserialize, Serialize};
/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Whether the service is healthy.
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    pub last_update_ms: u64,
    /// Number of pending tasks in queue.
    pub queue_depth: usize,
}
