use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

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
    #[serde(default = "num_cpus::get")]
    pub num_workers: usize,
    /// Task queue capacity for this pool
    #[serde(default = "usize_val::<1000>")]
    pub task_queue_capacity: usize,
    /// Minimum hops to search (must be >= 1)
    #[serde(default = "usize_val::<1>")]
    pub min_hops: usize,
    /// Maximum hops to search
    #[serde(default = "usize_val::<3>")]
    pub max_hops: usize,
    /// Timeout for solving in milliseconds
    #[serde(default = "u64_val::<100>")]
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

/// Blacklist configuration for filtering components.
///
/// Components in this config will be excluded from routing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlacklistConfig {
    /// Component IDs to exclude (e.g., pool addresses with simulation issues).
    #[serde(default)]
    pub components: HashSet<String>,
}

impl BlacklistConfig {
    /// Load blacklist configuration from a TOML file.
    ///
    /// The TOML file should have a `[blacklist]` section:
    /// ```toml
    /// [blacklist]
    /// components = ["0x86d257cdb7bc9c0df10e84c8709697f92770b335"]
    /// ```
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read blacklist config {}", path.display()))?;

        #[derive(Deserialize)]
        struct Wrapper {
            blacklist: BlacklistConfig,
        }

        let wrapper: Wrapper = toml::from_str(&contents)
            .with_context(|| format!("failed to parse blacklist config {}", path.display()))?;
        Ok(wrapper.blacklist)
    }
}

// Worker defaults

fn usize_val<const V: usize>() -> usize {
    V
}

fn u64_val<const V: u64>() -> u64 {
    V
}

// Solver defaults
pub(crate) mod defaults {
    pub const HTTP_HOST: &str = "0.0.0.0";
    pub const HTTP_PORT: u16 = 3000;
    pub const MIN_TVL: f64 = 10.0;
    pub const MIN_TOKEN_QUALITY: i32 = 100;
    pub const TVL_BUFFER_MULTIPLIER: f64 = 1.1;
    pub const GAS_REFRESH_INTERVAL_SECS: u64 = 30;
    pub const RECONNECT_DELAY_SECS: u64 = 5;
    pub const ORDER_MANAGER_TIMEOUT_MS: u64 = 100;
    pub const ORDER_MANAGER_MIN_RESPONSES: usize = 0;
    /// Slippage threshold for pool depth computation (1%)
    pub const DEPTH_SLIPPAGE_THRESHOLD: f64 = 0.01;
}
