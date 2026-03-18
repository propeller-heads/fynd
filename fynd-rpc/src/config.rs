use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
pub use fynd_core::PoolConfig;
use serde::{Deserialize, Serialize};

/// Worker pools configuration loaded from TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerPoolsConfig {
    /// Pool configurations (at least one pool must be specified)
    pub pools: HashMap<String, PoolConfig>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_config_minimal_uses_defaults() {
        let toml = r#"
            [pools.basic]
            algorithm = "most_liquid"
        "#;
        let config: WorkerPoolsConfig = toml::from_str(toml).unwrap();
        let pool = &config.pools["basic"];
        assert_eq!(pool.algorithm, "most_liquid");
        assert_eq!(pool.num_workers, num_cpus::get());
        assert_eq!(pool.task_queue_capacity, defaults::POOL_TASK_QUEUE_CAPACITY);
        assert_eq!(pool.min_hops, defaults::POOL_MIN_HOPS);
        assert_eq!(pool.max_hops, defaults::POOL_MAX_HOPS);
        assert_eq!(pool.timeout_ms, defaults::POOL_TIMEOUT_MS);
        assert_eq!(pool.max_routes, None);
    }

    #[test]
    fn test_pool_config_all_fields_explicit() {
        let toml = r#"
            [pools.custom]
            algorithm = "most_liquid"
            num_workers = 8
            task_queue_capacity = 500
            min_hops = 2
            max_hops = 4
            timeout_ms = 200
            max_routes = 50
        "#;
        let config: WorkerPoolsConfig = toml::from_str(toml).unwrap();
        let pool = &config.pools["custom"];
        assert_eq!(pool.algorithm, "most_liquid");
        assert_eq!(pool.num_workers, 8);
        assert_eq!(pool.task_queue_capacity, 500);
        assert_eq!(pool.min_hops, 2);
        assert_eq!(pool.max_hops, 4);
        assert_eq!(pool.timeout_ms, 200);
        assert_eq!(pool.max_routes, Some(50));
    }
}

pub mod defaults {
    // HTTP server
    pub const HTTP_HOST: &str = "0.0.0.0";
    pub const HTTP_PORT: u16 = 3000;

    // RPC
    pub const DEFAULT_RPC_URL: &str = "https://eth.llamarpc.com";

    // Tycho stream
    pub const MIN_TVL: f64 = 10.0;
    pub const MIN_TOKEN_QUALITY: i32 = 100;
    pub const TRADED_N_DAYS_AGO: u64 = 3;
    pub const TVL_BUFFER_RATIO: f64 = 1.1;
    pub const RECONNECT_DELAY_SECS: u64 = 5;

    // Gas
    pub const GAS_REFRESH_INTERVAL_SECS: u64 = 30;

    // Worker router
    pub const WORKER_ROUTER_TIMEOUT_MS: u64 = 100;
    pub const WORKER_ROUTER_MIN_RESPONSES: usize = 0;

    // Derived data
    pub const DEPTH_SLIPPAGE_THRESHOLD: f64 = 0.01;

    // Worker pool config
    pub const POOL_TASK_QUEUE_CAPACITY: usize = 1000;
    pub const POOL_MIN_HOPS: usize = 1;
    pub const POOL_MAX_HOPS: usize = 3;
    pub const POOL_TIMEOUT_MS: u64 = 100;

    /// Returns the default Tycho Fynd endpoint URL for the given chain.
    ///
    /// Returns an error if the chain is not recognized.
    pub fn default_tycho_url(chain: &str) -> Result<&str, String> {
        match chain.to_lowercase().as_str() {
            "ethereum" => Ok("tycho-fynd-ethereum.propellerheads.xyz"),
            "base" => Ok("tycho-fynd-base.propellerheads.xyz"),
            "unichain" => Ok("tycho-fynd-unichain.propellerheads.xyz"),
            other => Err(format!(
                "no default Tycho URL for chain '{}'. Please provide --tycho-url explicitly.",
                other
            )),
        }
    }
}
