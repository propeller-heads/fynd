//! API request and response types.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthStatus {
    /// Whether the service is healthy.
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    pub last_update_ms: u64,
    /// Number of active solver pools.
    pub num_solver_pools: usize,
}
