use std::{collections::HashMap, fs, path::Path};

use anyhow::{Context, Result};
use num_cpus;
use serde::{Deserialize, Serialize};

/// Worker pools configuration loaded from TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerPoolsConfig {
    /// Pool configurations (at least one pool must be specified)
    pub pools: HashMap<String, PoolConfig>,
}

/// Per-pool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Algorithm name for this pool (e.g., "most_liquid", "dijkstra")
    pub algorithm: String,
    /// Number of worker threads for this pool
    #[serde(default = "default_workers_per_pool")]
    pub num_workers: usize,
    /// Task queue capacity for this pool
    #[serde(default = "default_task_queue_capacity")]
    pub task_queue_capacity: usize,
    /// Minimum hops to search (must be >= 1)
    #[serde(default = "default_min_hops")]
    pub min_hops: usize,
    /// Maximum hops to search
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
    /// Timeout for solving in milliseconds
    #[serde(default = "default_worker_timeout_ms")]
    pub timeout_ms: u64,
}

impl WorkerPoolsConfig {
    /// Load worker pools configuration from a TOML file.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file {}", path.display()))
    }
}

// Worker defaults

fn default_workers_per_pool() -> usize {
    num_cpus::get()
}

const fn default_task_queue_capacity() -> usize {
    1000
}

const fn default_min_hops() -> usize {
    1
}

const fn default_max_hops() -> usize {
    3
}

const fn default_worker_timeout_ms() -> u64 {
    100
}

// Solver defaults
pub mod defaults {
    pub const HTTP_HOST: &str = "0.0.0.0";
    pub const HTTP_PORT: u16 = 3000;
    pub const MIN_TVL: f64 = 10.0;
    pub const TVL_BUFFER_MULTIPLIER: f64 = 1.1;
    pub const GAS_REFRESH_INTERVAL_SECS: u64 = 30;
    pub const RECONNECT_DELAY_SECS: u64 = 5;
    pub const ORDER_MANAGER_TIMEOUT_MS: u64 = 100;
    pub const ORDER_MANAGER_MIN_RESPONSES: usize = 0;
}
