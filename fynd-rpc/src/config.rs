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
    pools: HashMap<String, PoolConfig>,
}

impl WorkerPoolsConfig {
    /// Creates a new config from a pools map.
    pub fn new(pools: HashMap<String, PoolConfig>) -> Self {
        Self { pools }
    }

    /// Returns the pool configurations.
    pub fn pools(&self) -> &HashMap<String, PoolConfig> {
        &self.pools
    }

    /// Consumes the config and returns the pools map.
    pub fn into_pools(self) -> HashMap<String, PoolConfig> {
        self.pools
    }

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
    components: HashSet<String>,
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

    pub(crate) fn into_components(self) -> HashSet<String> {
        self.components
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
        let pool = &config.pools()["basic"];
        assert_eq!(pool.algorithm(), "most_liquid");
        assert_eq!(pool.num_workers(), num_cpus::get());
        assert_eq!(pool.task_queue_capacity(), defaults::POOL_TASK_QUEUE_CAPACITY);
        assert_eq!(pool.min_hops(), defaults::POOL_MIN_HOPS);
        assert_eq!(pool.max_hops(), defaults::POOL_MAX_HOPS);
        assert_eq!(pool.timeout_ms(), defaults::POOL_TIMEOUT_MS);
        assert_eq!(pool.max_routes(), None);
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
        let pool = &config.pools()["custom"];
        assert_eq!(pool.algorithm(), "most_liquid");
        assert_eq!(pool.num_workers(), 8);
        assert_eq!(pool.task_queue_capacity(), 500);
        assert_eq!(pool.min_hops(), 2);
        assert_eq!(pool.max_hops(), 4);
        assert_eq!(pool.timeout_ms(), 200);
        assert_eq!(pool.max_routes(), Some(50));
    }
}

/// Default values for all `fynd-rpc` configuration parameters.
pub mod defaults {
    // Re-export shared defaults from fynd-core as the single source of truth.
    pub use fynd_core::solver::defaults::{
        GAS_REFRESH_INTERVAL, MIN_TOKEN_QUALITY, POOL_MAX_HOPS, POOL_MIN_HOPS,
        POOL_TASK_QUEUE_CAPACITY, POOL_TIMEOUT_MS, RECONNECT_DELAY, ROUTER_MIN_RESPONSES,
        TRADED_N_DAYS_AGO, TVL_BUFFER_RATIO,
    };

    /// Default HTTP bind host (`"0.0.0.0"` — all interfaces).
    pub const HTTP_HOST: &str = "0.0.0.0";
    /// Default HTTP port (`3000`).
    pub const HTTP_PORT: u16 = 3000;

    /// Default Ethereum JSON-RPC URL used when none is provided.
    pub const DEFAULT_RPC_URL: &str = "https://eth.llamarpc.com";

    /// Minimum TVL a pool must have to be included in routing, denominated in the chain's native
    /// token.
    pub const MIN_TVL: f64 = 10.0;

    /// Worker-router timeout in milliseconds.
    ///
    /// Intentionally tighter than `FyndBuilder`'s generous 10 s standalone default; an HTTP
    /// service must respond within its request deadline.
    pub const WORKER_ROUTER_TIMEOUT_MS: u64 = 100;

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
