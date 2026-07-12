//! Collector configuration and validation.

use std::{collections::HashSet, path::Path, str::FromStr};

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::models::Address;

/// Complete collector configuration loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorConfig {
    /// Stable identifier for this configuration and output series.
    pub run_name: String,
    /// Fynd and Tycho runtime settings.
    pub fynd: FyndConfig,
    /// Collection and storage limits.
    pub collection: CollectionConfig,
    /// Token registry referenced by pairs.
    pub tokens: Vec<TokenConfig>,
    /// Direct execution pairs to sample.
    pub pairs: Vec<PairConfig>,
}

/// Fynd runtime settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FyndConfig {
    /// Development Tycho endpoint hostname.
    pub tycho_url: String,
    /// Environment variable containing the Tycho API key.
    pub tycho_api_key_env: String,
    /// Environment variable containing the HTTP Ethereum RPC URL.
    pub rpc_http_url_env: String,
    /// Environment variable containing the WebSocket Ethereum RPC URL.
    pub rpc_ws_url_env: String,
    /// Protocol systems included in the market universe.
    pub protocols: Vec<String>,
    /// Minimum TVL accepted by the Tycho feed.
    pub min_tvl: f64,
    /// Built-in routing algorithm.
    pub algorithm: String,
    /// Worker threads in the single collector pool.
    pub num_workers: usize,
    /// Maximum queued jobs in the worker pool.
    pub task_queue_capacity: usize,
    /// Maximum route hops.
    pub max_hops: usize,
    /// Per-algorithm timeout in milliseconds.
    pub algorithm_timeout_ms: u64,
}

/// Collection limits and persistence settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionConfig {
    /// Sender used for quote simulation.
    pub sender: String,
    /// Maximum orders submitted in one quote wave.
    pub request_chunk_size: usize,
    /// Deadline for Fynd to reach an RPC-observed head.
    pub state_wait_timeout_ms: u64,
    /// Timeout for one Fynd quote request.
    pub quote_timeout_ms: u64,
    /// Maximum wall time spent collecting one observed head.
    pub collection_budget_ms: u64,
    /// Confirmation depth used for canonical status events.
    pub confirmation_depth: u64,
}

/// Token metadata and fixed identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenConfig {
    /// Short stable ID used by pair configuration.
    pub id: String,
    /// Canonical EVM address.
    pub address: String,
    /// Display symbol only, never identity.
    pub symbol: String,
    /// ERC-20 decimal count.
    pub decimals: u8,
}

/// Direct pair and fixed integer grid for both exact-input directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairConfig {
    /// Stable pair ID.
    pub id: String,
    /// First token ID.
    pub token_a: String,
    /// Second token ID.
    pub token_b: String,
    /// Exact token-A input amounts in base units.
    pub amounts_a: Vec<String>,
    /// Exact token-B input amounts in base units.
    pub amounts_b: Vec<String>,
}

/// Configuration validation failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Configuration file could not be read.
    #[error("failed to read collector config {path}: {source}")]
    Read {
        /// Input path.
        path: String,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// TOML parsing failed.
    #[error("failed to parse collector config {path}: {source}")]
    Parse {
        /// Input path.
        path: String,
        /// TOML error.
        source: toml::de::Error,
    },
    /// A semantic invariant was violated.
    #[error("invalid collector config: {0}")]
    Invalid(String),
}

impl CollectorConfig {
    /// Load and validate a TOML configuration.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.display().to_string(), source })?;
        let config: Self = toml::from_str(&raw)
            .map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })?;
        config.validate()?;
        Ok(config)
    }

    /// Validate cross-field invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.run_name.trim().is_empty() {
            return Err(ConfigError::Invalid("run_name must not be empty".into()));
        }
        validate_runtime(&self.fynd, &self.collection)?;

        let mut token_ids = HashSet::new();
        let mut token_addresses = HashSet::new();
        for token in &self.tokens {
            if !token_ids.insert(token.id.as_str()) {
                return Err(ConfigError::Invalid(format!("duplicate token id: {}", token.id)));
            }
            let address = Address::from_str(&token.address).map_err(|error| {
                ConfigError::Invalid(format!("invalid address for token {}: {error}", token.id))
            })?;
            if !token_addresses.insert(address) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate token address: {}",
                    token.address
                )));
            }
        }
        if token_ids.len() < 2 {
            return Err(ConfigError::Invalid("at least two tokens are required".into()));
        }

        let mut pair_ids = HashSet::new();
        let mut pair_token_sets = HashSet::new();
        for pair in &self.pairs {
            validate_pair(pair, &token_ids, &mut pair_ids)?;
            let token_set = if pair.token_a <= pair.token_b {
                (pair.token_a.as_str(), pair.token_b.as_str())
            } else {
                (pair.token_b.as_str(), pair.token_a.as_str())
            };
            if !pair_token_sets.insert(token_set) {
                return Err(ConfigError::Invalid(format!(
                    "pairs {} and an earlier pair cover the same token set",
                    pair.id
                )));
            }
        }
        if self.pairs.is_empty() {
            return Err(ConfigError::Invalid("at least one pair is required".into()));
        }
        Ok(())
    }

    /// Maximum source and matched-reverse rows emitted for one block.
    pub fn expected_rows_per_block(&self) -> usize {
        self.pairs
            .iter()
            .map(|pair| (pair.amounts_a.len() + pair.amounts_b.len()) * 2)
            .sum()
    }

}

fn validate_runtime(fynd: &FyndConfig, collection: &CollectionConfig) -> Result<(), ConfigError> {
    let is_dev_endpoint = fynd.tycho_url.contains("beta") ||
        fynd.tycho_url.contains("dev") ||
        fynd.tycho_url.contains("localhost");
    if !is_dev_endpoint {
        return Err(ConfigError::Invalid(format!(
            "tycho_url must be a dev or beta endpoint, got {}",
            fynd.tycho_url
        )));
    }
    if fynd.protocols.is_empty() {
        return Err(ConfigError::Invalid("at least one protocol is required".into()));
    }
    if fynd.num_workers == 0 || fynd.task_queue_capacity == 0 || fynd.max_hops == 0 {
        return Err(ConfigError::Invalid(
            "num_workers, task_queue_capacity, and max_hops must be positive".into(),
        ));
    }
    if collection.request_chunk_size == 0 ||
        collection.state_wait_timeout_ms == 0 ||
        collection.quote_timeout_ms == 0 ||
        collection.collection_budget_ms == 0
    {
        return Err(ConfigError::Invalid(
            "chunk size and collection timeouts must be positive".into(),
        ));
    }
    if collection.collection_budget_ms < collection.quote_timeout_ms {
        return Err(ConfigError::Invalid(
            "collection_budget_ms must be at least quote_timeout_ms".into(),
        ));
    }
    if collection.request_chunk_size > fynd.task_queue_capacity {
        return Err(ConfigError::Invalid(format!(
            "request_chunk_size {} exceeds worker task_queue_capacity {}",
            collection.request_chunk_size, fynd.task_queue_capacity
        )));
    }
    Address::from_str(&collection.sender)
        .map_err(|error| ConfigError::Invalid(format!("invalid sender address: {error}")))?;
    Ok(())
}

fn validate_pair<'a>(
    pair: &'a PairConfig,
    token_ids: &HashSet<&'a str>,
    pair_ids: &mut HashSet<&'a str>,
) -> Result<(), ConfigError>
where
    PairConfig: 'a,
{
    if !pair_ids.insert(pair.id.as_str()) {
        return Err(ConfigError::Invalid(format!("duplicate pair id: {}", pair.id)));
    }
    if pair.token_a == pair.token_b {
        return Err(ConfigError::Invalid(format!(
            "pair {} references the same token twice",
            pair.id
        )));
    }
    for token_id in [&pair.token_a, &pair.token_b] {
        if !token_ids.contains(token_id.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "pair {} references unknown token {}",
                pair.id, token_id
            )));
        }
    }
    validate_amounts(pair, "amounts_a", &pair.amounts_a)?;
    validate_amounts(pair, "amounts_b", &pair.amounts_b)?;
    Ok(())
}

fn validate_amounts(pair: &PairConfig, field: &str, amounts: &[String]) -> Result<(), ConfigError> {
    if amounts.is_empty() {
        return Err(ConfigError::Invalid(format!("pair {} has empty {field}", pair.id)));
    }
    for amount in amounts {
        let parsed = BigUint::from_str(amount).map_err(|error| {
            ConfigError::Invalid(format!(
                "pair {} has invalid {field} value {amount}: {error}",
                pair.id
            ))
        })?;
        if parsed == BigUint::from(0u8) {
            return Err(ConfigError::Invalid(format!("pair {} has zero {field} value", pair.id)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> CollectorConfig {
        CollectorConfig {
            run_name: "pilot".into(),
            fynd: FyndConfig {
                tycho_url: "tycho-beta.propellerheads.xyz".into(),
                tycho_api_key_env: "TYCHO_API_KEY_BETA".into(),
                rpc_http_url_env: "RPC_URL".into(),
                rpc_ws_url_env: "RPC_WS_URL".into(),
                protocols: vec!["uniswap_v3".into()],
                min_tvl: 10.0,
                algorithm: "bellman_ford".into(),
                num_workers: 4,
                task_queue_capacity: 1_000,
                max_hops: 3,
                algorithm_timeout_ms: 500,
            },
            collection: CollectionConfig {
                sender: "0x0000000000000000000000000000000000000001".into(),
                request_chunk_size: 32,
                state_wait_timeout_ms: 3_000,
                quote_timeout_ms: 5_000,
                collection_budget_ms: 9_000,
                confirmation_depth: 12,
            },
            tokens: vec![
                TokenConfig {
                    id: "weth".into(),
                    address: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
                    symbol: "WETH".into(),
                    decimals: 18,
                },
                TokenConfig {
                    id: "usdc".into(),
                    address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
                    symbol: "USDC".into(),
                    decimals: 6,
                },
            ],
            pairs: vec![PairConfig {
                id: "weth-usdc".into(),
                token_a: "weth".into(),
                token_b: "usdc".into(),
                amounts_a: vec!["1000000000000000000".into()],
                amounts_b: vec!["3000000000".into()],
            }],
        }
    }

    #[test]
    fn rejects_duplicate_token_ids() {
        let mut config = valid_config();
        config
            .tokens
            .push(config.tokens[0].clone());

        let error = config
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("duplicate token id"));
    }

    #[test]
    fn rejects_unknown_pair_token() {
        let mut config = valid_config();
        config.pairs[0].token_b = "missing".into();

        let error = config
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown token"));
    }

    #[test]
    fn expected_rows_include_forward_and_matched_reverse_roles() {
        let mut config = valid_config();
        config.pairs[0].amounts_a = vec!["1".into(), "2".into()];
        config.pairs[0].amounts_b = vec!["3".into(), "4".into(), "5".into()];

        assert_eq!(config.expected_rows_per_block(), 10);
    }

    #[test]
    fn rejects_second_pair_over_the_same_token_set() {
        let mut config = valid_config();
        config.pairs.push(PairConfig {
            id: "usdc-weth-reversed".into(),
            token_a: "usdc".into(),
            token_b: "weth".into(),
            amounts_a: vec!["10".into()],
            amounts_b: vec!["20".into()],
        });

        let error = config
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("same token set"));
    }

    #[test]
    fn rejects_chunk_larger_than_worker_queue() {
        let mut config = valid_config();
        config.collection.request_chunk_size = 1_001;

        let error = config
            .validate()
            .unwrap_err()
            .to_string();

        assert!(error.contains("exceeds worker task_queue_capacity"));
    }
}
