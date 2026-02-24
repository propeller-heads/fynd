//! API request and response types.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthStatus {
    /// Whether the service is healthy.
    #[schema(example = true)]
    pub healthy: bool,
    /// Time since last market update in milliseconds.
    #[schema(example = 1250)]
    pub last_update_ms: u64,
    /// Number of active solver pools.
    #[schema(example = 2)]
    pub num_solver_pools: usize,
}
