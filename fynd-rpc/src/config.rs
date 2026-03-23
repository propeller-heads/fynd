use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
pub use fynd_core::PoolConfig;
use serde::{Deserialize, Serialize};

/// The default worker pools configuration embedded at compile time.
///
/// Used as a fallback when the default `worker_pools.toml` path is not found at runtime,
/// so the binary works out-of-the-box without a config file (e.g. `cargo install`, Docker).
///
/// Keep in sync with the repo-root `worker_pools.toml` (the user-facing example config).
/// Cannot use `include_str!` here because `cargo publish` verifies the crate in isolation,
/// and the file lives outside the `fynd-rpc` package directory.
const DEFAULT_WORKER_POOLS_TOML: &str = r#"
[pools.bellman_ford_2_hops]
algorithm = "bellman_ford"
num_workers = 3
task_queue_capacity = 1000
max_hops = 2
timeout_ms = 500
"#;

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

    /// Returns the built-in default configuration embedded in the binary.
    ///
    /// This is the repo-root `worker_pools.toml` baked in at compile time.
    pub fn builtin_default() -> Self {
        toml::from_str(DEFAULT_WORKER_POOLS_TOML).expect("built-in worker_pools.toml is valid TOML")
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

/// Blocklist configuration for excluding components from the Tycho stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlocklistConfig {
    /// Component IDs to exclude (e.g., pool addresses with simulation issues).
    #[serde(default)]
    components: HashSet<String>,
}

/// The default blacklist configuration embedded at compile time.
///
/// Keep in sync with the repo-root `blacklist.toml` (the user-facing example config).
/// Cannot use `include_str!` here because `cargo publish` verifies the crate in isolation,
/// and the file lives outside the `fynd-rpc` package directory.
const DEFAULT_BLACKLIST_TOML: &str = r#"
[blacklist]
components = [
    # AMPL pools - AMPL is a rebasing token that breaks simulation assumptions
    # UniswapV3 AMPL/WETH
    "0x86d257cdb7bc9c0df10e84c8709697f92770b335",
    # UniswapV2 AMPL/WETH
    "0xc5be99a02c6857f9eac67bbce58df5572498f40c",
    # Fluid Lite pools with broken simulation (ENG-5696)
    "0x32aa6f5c6f771b39d383c6e36d7ef0702a28e32ad64671c059f41547b82ef0ef",
    "0x7f31b44f032f125bb465c161343fccfdc88fee8dc94c068f6430d2345d80f1d1",
    # Curve yETH/WETH — exploited Nov 2025 ($9M), broken invariant, pool insolvent
    "0x69accb968b19a53790f43e57558f5e443a91af22",
    # Fluid syrupUSDC/USDC — ERC-4626 vault token, simulation can't track accumulating rate
    "0x79eea4a1be86c43a9a9c4384b0b28a07af24ae29",
]
"#;

impl BlacklistConfig {
    /// Returns the built-in default blacklist embedded in the binary.
    ///
    /// This is the repo-root `blacklist.toml` baked in at compile time.
    pub fn builtin_default() -> Self {
        #[derive(Deserialize)]
        struct Wrapper {
            blacklist: BlacklistConfig,
        }
        let wrapper: Wrapper =
            toml::from_str(DEFAULT_BLACKLIST_TOML).expect("built-in blacklist.toml is valid TOML");
        wrapper.blacklist
    }

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
            .with_context(|| format!("failed to read blocklist config {}", path.display()))?;

        #[derive(Deserialize)]
        struct Wrapper {
            blacklist: BlocklistConfig,
        }

        let wrapper: Wrapper = toml::from_str(&contents)
            .with_context(|| format!("failed to parse blocklist config {}", path.display()))?;
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
    fn test_builtin_default_does_not_panic() {
        let config = WorkerPoolsConfig::builtin_default();
        assert!(!config.pools().is_empty(), "built-in config must have at least one pool");
    }

    #[test]
    fn test_blacklist_builtin_default_does_not_panic() {
        let config = BlacklistConfig::builtin_default();
        assert!(
            !config.components.is_empty(),
            "built-in blacklist must have at least one component"
        );
    }

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
        use fynd_core::solver::defaults as core_defaults;
        assert_eq!(pool.task_queue_capacity(), core_defaults::POOL_TASK_QUEUE_CAPACITY);
        assert_eq!(pool.min_hops(), core_defaults::POOL_MIN_HOPS);
        assert_eq!(pool.max_hops(), core_defaults::POOL_MAX_HOPS);
        assert_eq!(pool.timeout_ms(), core_defaults::POOL_TIMEOUT_MS);
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
        GAS_REFRESH_INTERVAL, MIN_TOKEN_QUALITY, RECONNECT_DELAY, ROUTER_MIN_RESPONSES,
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
