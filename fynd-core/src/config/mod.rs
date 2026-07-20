//! Layered solver configuration.
//!
//! Every solver-tuning field resolves independently through three layers, highest priority
//! first:
//!
//! 1. **Explicit overrides** — lib builder setters or CLI flags
//! 2. **Local config file** — any subset of the fields, same schema as the embedded default
//! 3. **Remote config** — tuned values pulled from S3 per chain (see [`remote`])
//! 4. **Embedded default** — `default_config.toml`, compiled into the binary
//!
//! The embedded default deserializes directly into a complete [`Config`] — every field
//! (except the chain-specific `min_tvl`) is required, so a gap between the struct and
//! `default_config.toml` cannot go unnoticed. The other layers each produce a
//! [`PartialConfig`] applied on top with [`Config::apply`], field by field, in ascending
//! priority order. The final result is validated as a whole ([`Config::validate`]), not
//! per layer; [`FyndBuilder::apply_config`](crate::solver::FyndBuilder::apply_config)
//! validates automatically.
//!
//! Collections (`protocols`, `pools`) merge atomically: a layer either sets the whole
//! list/map or nothing.
//!
//! # Example
//!
//! ```ignore
//! use fynd_core::config::{embedded_default, PartialConfig};
//!
//! let overrides = PartialConfig { worker_router_timeout_ms: Some(50), ..Default::default() };
//! let config = embedded_default()
//!     .clone()
//!     .apply_remote(&remote::default_remote_config_url(chain), timeout)
//!     .await
//!     .apply(&PartialConfig::from_file("fynd.toml")?)
//!     .apply(&overrides);
//! let builder = FyndBuilder::new(chain, tycho_url, rpc_url, config.protocols.clone(), min_tvl)
//!     .apply_config(&config)?; // validates the config
//! ```

use std::{collections::HashMap, path::Path, sync::LazyLock};

use serde::Deserialize;
use tycho_simulation::tycho_common::models::{Chain, TvlThresholdTier};

use crate::solver::PoolConfig;

pub mod remote;

/// The embedded default configuration, compiled into the binary.
const EMBEDDED_DEFAULT_TOML: &str = include_str!("default_config.toml");

/// A fully resolved solver configuration — no optional fields, ready for the engine.
///
/// Start from [`embedded_default`], overlay [`PartialConfig`] layers with [`Config::apply`],
/// and consume via [`FyndBuilder::apply_config`](crate::solver::FyndBuilder::apply_config).
///
/// Deserialization is only used for the embedded default and requires every field (except
/// `min_tvl`): adding a field here without updating `default_config.toml` fails parsing,
/// which unit tests catch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Minimum TVL threshold in native token units (e.g. ETH). Components below this are
    /// excluded from routing. `None` means the chain's default TVL threshold — the one
    /// chain-specific value, which is why the embedded default leaves it unset. Resolve it
    /// with [`Config::min_tvl_or_chain_default`].
    #[serde(default)]
    pub min_tvl: Option<f64>,
    /// Multiplier defining the lower hysteresis bound of the TVL filter (must be >= 1.0).
    pub tvl_buffer_ratio: f64,
    /// Minimum token quality score required for a token to be included in routing.
    pub min_token_quality: i32,
    /// Only include tokens traded within this many days.
    pub traded_n_days_ago: u64,
    /// How often the gas price is refreshed from the RPC node, in seconds.
    pub gas_refresh_interval_secs: u64,
    /// Delay before reconnecting to the Tycho feed after a disconnect, in seconds.
    pub reconnect_delay_secs: u64,
    /// Worker router timeout in milliseconds.
    pub worker_router_timeout_ms: u64,
    /// Minimum solver responses before early return (`0` = wait for all pools).
    pub worker_router_min_responses: usize,
    /// Enable partial block (flashblock) updates from the Tycho stream.
    pub partial_blocks: bool,
    /// Protocols to index (e.g. `"uniswap_v2"`). The fynd binary also accepts the
    /// `"all_onchain"` placeholder, expanded to every on-chain protocol available from
    /// Tycho RPC (the embedded default uses it).
    pub protocols: Vec<String>,
    /// Worker pool definitions, keyed by pool name.
    pub pools: HashMap<String, PoolConfig>,
}

/// One layer's contribution to the configuration: every field is optional.
///
/// All layers — builder/CLI overrides, the local config file, and the embedded default —
/// deserialize into this same type. Unknown keys are ignored, so a config file written for
/// a newer binary still parses on an older one, which only applies the fields it knows.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PartialConfig {
    /// See [`Config::min_tvl`].
    pub min_tvl: Option<f64>,
    /// See [`Config::tvl_buffer_ratio`].
    pub tvl_buffer_ratio: Option<f64>,
    /// See [`Config::min_token_quality`].
    pub min_token_quality: Option<i32>,
    /// See [`Config::traded_n_days_ago`].
    pub traded_n_days_ago: Option<u64>,
    /// See [`Config::gas_refresh_interval_secs`].
    pub gas_refresh_interval_secs: Option<u64>,
    /// See [`Config::reconnect_delay_secs`].
    pub reconnect_delay_secs: Option<u64>,
    /// See [`Config::worker_router_timeout_ms`].
    pub worker_router_timeout_ms: Option<u64>,
    /// See [`Config::worker_router_min_responses`].
    pub worker_router_min_responses: Option<usize>,
    /// See [`Config::partial_blocks`].
    pub partial_blocks: Option<bool>,
    /// See [`Config::protocols`]. Merged atomically: a layer either sets the whole list or
    /// nothing.
    pub protocols: Option<Vec<String>>,
    /// See [`Config::pools`]. Merged atomically: a layer either sets the whole map or
    /// nothing.
    pub pools: Option<HashMap<String, PoolConfig>>,
}

impl PartialConfig {
    /// Parses a config layer from a TOML string.
    ///
    /// `source` names the origin (e.g. a file path) for error messages. Unknown fields are
    /// ignored (see the type-level docs).
    pub fn from_toml_str(raw: &str, source: &str) -> Result<Self, ConfigError> {
        toml::from_str(raw)
            .map_err(|e| ConfigError::Parse { context: source.to_string(), source: e })
    }

    /// Reads and parses a config layer from a TOML file. See [`Self::from_toml_str`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::ReadFile { path: path.display().to_string(), source: e })?;
        Self::from_toml_str(&raw, &path.display().to_string())
    }
}

/// The embedded default configuration, parsed once on first use.
static EMBEDDED_DEFAULT: LazyLock<Config> = LazyLock::new(|| {
    toml::from_str(EMBEDDED_DEFAULT_TOML)
        .expect("embedded default_config.toml is a complete, valid Config; checked by unit tests")
});

/// Returns the embedded default configuration (`default_config.toml`), parsed once and
/// cached.
///
/// This is the single source of truth for all solver-tuning defaults. The TOML
/// deserializes directly into a complete [`Config`]; every field is required except the
/// chain-specific `min_tvl` (see [`Config::min_tvl`]).
pub fn embedded_default() -> &'static Config {
    &EMBEDDED_DEFAULT
}

/// Overall time budget (including retries) for the fetch inside [`get_default`].
/// Callers wanting a different budget use [`Config::apply_remote`] directly.
const GET_DEFAULT_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Returns the embedded default configuration with the latest remotely tuned values for
/// `chain` applied on top, fetched from the default S3 URL (see
/// [`remote::default_remote_config_url`]).
///
/// The simple one-call form of `embedded_default().clone().apply_remote(...)`, with a
/// built-in 2 s fetch budget. Never fails or panics: on any fetch problem the embedded
/// defaults are returned unchanged (a warning is logged). Layer local overrides on top
/// with [`Config::apply`]; for a custom URL or timeout use [`Config::apply_remote`].
pub async fn get_default(chain: Chain) -> Config {
    embedded_default()
        .clone()
        .apply_remote(&remote::default_remote_config_url(chain), GET_DEFAULT_FETCH_TIMEOUT)
        .await
}

impl Config {
    /// Returns `min_tvl`, falling back to `chain`'s default TVL threshold when unset.
    pub fn min_tvl_or_chain_default(&self, chain: Chain) -> f64 {
        self.min_tvl
            .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Low))
    }

    /// Applies a partial layer on top of this config: fields the layer sets replace the
    /// current values; collections (`protocols`, `pools`) are replaced atomically.
    #[must_use]
    pub fn apply(mut self, partial: &PartialConfig) -> Self {
        // Exhaustive destructuring (no `..`): adding a field to `PartialConfig` without
        // handling it here is a compile error.
        let PartialConfig {
            min_tvl,
            tvl_buffer_ratio,
            min_token_quality,
            traded_n_days_ago,
            gas_refresh_interval_secs,
            reconnect_delay_secs,
            worker_router_timeout_ms,
            worker_router_min_responses,
            partial_blocks,
            protocols,
            pools,
        } = partial;
        if let Some(value) = min_tvl {
            self.min_tvl = Some(*value);
        }
        if let Some(value) = tvl_buffer_ratio {
            self.tvl_buffer_ratio = *value;
        }
        if let Some(value) = min_token_quality {
            self.min_token_quality = *value;
        }
        if let Some(value) = traded_n_days_ago {
            self.traded_n_days_ago = *value;
        }
        if let Some(value) = gas_refresh_interval_secs {
            self.gas_refresh_interval_secs = *value;
        }
        if let Some(value) = reconnect_delay_secs {
            self.reconnect_delay_secs = *value;
        }
        if let Some(value) = worker_router_timeout_ms {
            self.worker_router_timeout_ms = *value;
        }
        if let Some(value) = worker_router_min_responses {
            self.worker_router_min_responses = *value;
        }
        if let Some(value) = partial_blocks {
            self.partial_blocks = *value;
        }
        if let Some(value) = protocols {
            self.protocols = value.clone();
        }
        if let Some(value) = pools {
            self.pools = value.clone();
        }
        self
    }
}

/// Errors from parsing, resolving, or validating the layered configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A config file could not be read from disk.
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        /// Path that could not be read.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A layer failed to deserialize (malformed TOML).
    #[error("failed to parse {context}: {source}")]
    Parse {
        /// Origin of the payload (file path or "embedded").
        context: String,
        /// Underlying TOML error.
        #[source]
        source: toml::de::Error,
    },
    /// The final resolved configuration failed validation.
    #[error("invalid resolved config: {reasons}")]
    Validation {
        /// All validation failures, joined with "; ".
        reasons: String,
    },
}

impl Config {
    /// Range-checks the fully resolved configuration.
    ///
    /// Call this after the final [`apply`](Self::apply) — validation is meant for the
    /// resolved result, not individual layers, so a layer may set a value that only
    /// becomes invalid (or valid) in combination with lower layers.
    /// [`FyndBuilder::apply_config`](crate::solver::FyndBuilder::apply_config) calls it
    /// automatically.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] listing every violated range check.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let issues = validate(self);
        if issues.is_empty() {
            return Ok(());
        }
        Err(ConfigError::Validation { reasons: issues.join("; ") })
    }
}

/// Range-checks the final resolved configuration. Returns one message per violation.
fn validate(config: &Config) -> Vec<String> {
    let mut issues = Vec::new();
    if let Some(min_tvl) = config.min_tvl {
        if !min_tvl.is_finite() || min_tvl < 0.0 {
            issues.push(format!("min_tvl must be a non-negative finite number, got {min_tvl}"));
        }
    }
    if !config.tvl_buffer_ratio.is_finite() || config.tvl_buffer_ratio < 1.0 {
        issues.push(format!("tvl_buffer_ratio must be >= 1.0, got {}", config.tvl_buffer_ratio));
    }
    if config.gas_refresh_interval_secs == 0 {
        issues.push("gas_refresh_interval_secs must be > 0".to_string());
    }
    if config.reconnect_delay_secs == 0 {
        issues.push("reconnect_delay_secs must be > 0".to_string());
    }
    if config.worker_router_timeout_ms == 0 {
        issues.push("worker_router_timeout_ms must be > 0".to_string());
    }
    if config.pools.is_empty() {
        issues.push("at least one worker pool must be configured".to_string());
    }
    for (name, pool) in &config.pools {
        if pool.num_workers() == 0 {
            issues.push(format!("pool '{name}': num_workers must be > 0"));
        }
        if pool.task_queue_capacity() == 0 {
            issues.push(format!("pool '{name}': task_queue_capacity must be > 0"));
        }
        if pool.min_hops() == 0 {
            issues.push(format!("pool '{name}': min_hops must be >= 1"));
        }
        if pool.min_hops() > pool.max_hops() {
            issues.push(format!(
                "pool '{name}': min_hops ({}) must not exceed max_hops ({})",
                pool.min_hops(),
                pool.max_hops()
            ));
        }
        if pool.timeout_ms() == 0 {
            issues.push(format!("pool '{name}': timeout_ms must be > 0"));
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_default_is_complete_and_valid() {
        let config = embedded_default();
        config
            .validate()
            .expect("embedded default failed validation");
        assert!(config
            .pools
            .contains_key("bellman_ford_2_hops"));
        // min_tvl is the one chain-specific field: unset until resolved against a chain.
        assert_eq!(config.min_tvl, None);
        assert_eq!(
            config.min_tvl_or_chain_default(Chain::Ethereum),
            Chain::Ethereum.default_tvl_threshold(TvlThresholdTier::Low)
        );
    }

    #[test]
    fn test_resolve_precedence() {
        let overrides = PartialConfig {
            worker_router_timeout_ms: Some(50),
            min_token_quality: Some(90),
            ..PartialConfig::default()
        };
        let local_file = PartialConfig {
            min_token_quality: Some(80),
            traded_n_days_ago: Some(7),
            ..PartialConfig::default()
        };
        // Ascending priority: embedded default, then local config file, then overrides.
        let config = embedded_default()
            .clone()
            .apply(&local_file)
            .apply(&overrides);
        config
            .validate()
            .expect("validation errored");

        assert_eq!(config.worker_router_timeout_ms, 50);
        assert_eq!(config.min_token_quality, 90);
        assert_eq!(config.traded_n_days_ago, 7);
        assert_eq!(config.tvl_buffer_ratio, 1.1);
    }

    #[test]
    fn test_resolve_collections_merge_atomically() {
        let mut pools = HashMap::new();
        pools.insert("custom".to_string(), PoolConfig::new("most_liquid"));
        let local_file = PartialConfig {
            pools: Some(pools),
            protocols: Some(vec!["curve".to_string()]),
            ..PartialConfig::default()
        };
        let config = embedded_default()
            .clone()
            .apply(&local_file);
        // The whole map/list is replaced, not merged entry by entry.
        assert_eq!(config.pools.len(), 1);
        assert_eq!(config.pools["custom"].algorithm(), "most_liquid");
        assert_eq!(config.protocols, vec!["curve"]);
    }

    #[test]
    fn test_min_tvl_override() {
        let overrides = PartialConfig { min_tvl: Some(42.0), ..PartialConfig::default() };
        let config = embedded_default()
            .clone()
            .apply(&overrides);
        assert_eq!(config.min_tvl, Some(42.0));
    }

    #[test]
    fn test_partial_from_toml_ignores_unknown_fields() {
        // Permissive parsing: config files written for newer binaries (with fields this
        // version doesn't know) must still parse, applying only the known fields.
        let partial =
            PartialConfig::from_toml_str("min_tvl = 5.0\nfield_from_the_future = 1", "test.toml")
                .expect("parse errored");
        assert_eq!(partial.min_tvl, Some(5.0));
        assert_eq!(partial.min_token_quality, None);
    }

    #[test]
    fn test_partial_from_toml_parses_fields_and_pools() {
        let partial = PartialConfig::from_toml_str(
            r#"
            min_tvl = 20.0
            worker_router_timeout_ms = 150

            [pools.quick]
            algorithm = "most_liquid"
            max_hops = 2
            "#,
            "test.toml",
        )
        .expect("parse errored");
        assert_eq!(partial.min_tvl, Some(20.0));
        assert_eq!(partial.worker_router_timeout_ms, Some(150));
        let pools = partial.pools.expect("pools missing");
        assert_eq!(pools["quick"].algorithm(), "most_liquid");
        assert_eq!(pools["quick"].max_hops(), 2);
    }

    #[test]
    fn test_partial_from_toml_rejects_malformed_toml() {
        let error = PartialConfig::from_toml_str("min_tvl = [", "broken.toml").unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }));
        assert!(error
            .to_string()
            .contains("broken.toml"));
    }

    #[test]
    fn test_validation_runs_on_resolved_config() {
        let overrides = PartialConfig { tvl_buffer_ratio: Some(0.5), ..PartialConfig::default() };
        let error = embedded_default()
            .clone()
            .apply(&overrides)
            .validate()
            .unwrap_err();
        assert!(matches!(error, ConfigError::Validation { .. }));
        assert!(error
            .to_string()
            .contains("tvl_buffer_ratio"));
    }

    #[test]
    fn test_validation_pool_checks() {
        let mut pools = HashMap::new();
        pools.insert(
            "bad".to_string(),
            PoolConfig::new("bellman_ford")
                .with_num_workers(0)
                .with_min_hops(3)
                .with_max_hops(2),
        );
        let overrides = PartialConfig { pools: Some(pools), ..PartialConfig::default() };
        let error = embedded_default()
            .clone()
            .apply(&overrides)
            .validate()
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("num_workers"));
        assert!(message.contains("min_hops"));
    }
}
