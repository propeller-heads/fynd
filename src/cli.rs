use std::path::PathBuf;

use clap::Parser;
use fynd_core::config::{embedded_default, PartialConfig, Preset};
use fynd_rpc::{
    config::{defaults, WorkerPoolsConfig},
    parse_chain,
};
use tycho_simulation::tycho_common::models::Chain;

#[cfg(feature = "metrics")]
pub(crate) const METRICS_PORT: u16 = 9898;

use crate::commands::derive_connector_tokens::DeriveConnectorTokensArgs;

/// Builds help text for a config-layered flag, appending the real default value pulled
/// from the embedded default config (used when neither the CLI nor a config file sets the
/// field). Shown values are the `balanced` preset's; other presets may differ.
fn cfg_help(text: &str, default: impl std::fmt::Display) -> String {
    format!("{text} [default: {default}]")
}

/// Help text for `--preset`, listing the possible values derived from the enum.
fn preset_help() -> String {
    let names: Vec<&str> = Preset::all()
        .iter()
        .map(|preset| preset.as_str())
        .collect();
    format!(
        "Tuning preset the default config (embedded and remote) is maintained for [possible values: {}]",
        names.join(", ")
    )
}

/// Fynd - High-performance DEX solver built on Tycho
///
/// Finds optimal swap routes across multiple protocols using real-time market data.
#[derive(Parser, PartialEq, Debug)]
#[command(name = "fynd", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Available subcommands.
#[derive(clap::Subcommand, PartialEq, Debug)]
pub enum Commands {
    /// Run the solver HTTP server
    Serve(Box<ServeArgs>),
    /// Print the OpenAPI spec as JSON to stdout
    Openapi,
    /// Analyze live Tycho market data and suggest connector tokens for routing
    DeriveConnectorTokens(DeriveConnectorTokensArgs),
}

/// Arguments for the `serve` subcommand.
///
/// Solver-tuning flags resolve field-by-field through four layers, highest priority first:
/// explicit CLI flags, the local config file (`--config-file`, default `fynd.toml` if
/// present), the remote config pulled from S3, and the default config embedded in the
/// binary.
#[derive(clap::Args, PartialEq, Debug)]
pub struct ServeArgs {
    /// Target chain (e.g. Ethereum)
    #[arg(short, long, default_value = "Ethereum", value_parser = parse_chain)]
    pub chain: Chain,

    /// Tuning preset selecting which defaults apply (embedded and remote)
    #[arg(long, env, default_value = "balanced", help = preset_help())]
    pub preset: Preset,

    /// Path to a local TOML config file overriding the embedded defaults. Any subset of
    /// the config fields, same schema as the embedded default config.
    /// When omitted, ./fynd.toml is used if present.
    #[arg(long, env)]
    pub config_file: Option<PathBuf>,

    /// URL of the remote config pulled at startup. Defaults to the chain-specific
    /// PropellerHeads S3 URL. Fetch failures never block startup.
    #[arg(long, env)]
    pub remote_config_url: Option<String>,

    /// Disable fetching the remote config; resolve from CLI, local file, and embedded
    /// defaults only
    #[arg(long)]
    pub no_remote_config: bool,

    /// HTTP host (e.g. 0.0.0.0)
    #[arg(long, default_value = defaults::HTTP_HOST, env)]
    pub http_host: String,

    /// HTTP port
    #[arg(long, default_value_t = defaults::HTTP_PORT, env)]
    pub http_port: u16,

    /// Tycho URL. Defaults to the Fynd endpoint for the selected chain.
    #[arg(long, env)]
    pub tycho_url: Option<String>,

    /// Tycho API key
    #[arg(long, env)]
    pub tycho_api_key: Option<String>,

    /// Disable TLS for Tycho connection
    #[arg(long)]
    pub disable_tls: bool,

    /// Node RPC URL for the target chain. Defaults to a public endpoint if not set.
    #[arg(long, env)]
    pub rpc_url: Option<String>,

    /// List of protocols to index (comma-separated, e.g., uniswap_v2,uniswap_v3).
    #[arg(short, long, value_delimiter = ',', value_name = "PROTO1,PROTO2",
        help = cfg_help(
            "List of protocols to index (comma-separated). \"all_onchain\" expands to all \
             on-chain protocols fetched from Tycho RPC and can be combined with explicit \
             entries, e.g. all_onchain,rfq:bebop",
            embedded_default(Preset::Balanced).protocols.join(","),
        ))]
    pub protocols: Vec<String>,

    /// Minimum TVL threshold in native token (e.g. ETH). Components below this threshold
    /// will be removed from the market data. Defaults to a chain-specific value when no
    /// config layer sets it.
    #[arg(long)]
    pub min_tvl: Option<f64>,

    /// TVL buffer ratio.
    #[arg(long,
        help = cfg_help(
            "TVL buffer ratio: avoids fluctuations from components hovering around a single \
             threshold. With ratio 1.1 and minimum TVL 10 ETH, components are added when \
             TVL >= 10 ETH and removed below 10 / 1.1 ≈ 9.09 ETH",
            embedded_default(Preset::Balanced).tvl_buffer_ratio,
        ))]
    pub tvl_buffer_ratio: Option<f64>,

    /// Minimum token quality filter.
    #[arg(long, help = cfg_help("Minimum token quality filter", embedded_default(Preset::Balanced).min_token_quality))]
    pub min_token_quality: Option<i32>,

    /// Only include tokens traded within this many days.
    #[arg(long, help = cfg_help("Only include tokens traded within this many days", embedded_default(Preset::Balanced).traded_n_days_ago))]
    pub traded_n_days_ago: Option<u64>,

    /// Gas price refresh interval in seconds.
    #[arg(long, help = cfg_help("Gas price refresh interval in seconds", embedded_default(Preset::Balanced).gas_refresh_interval_secs))]
    pub gas_refresh_interval_secs: Option<u64>,

    /// Reconnect delay on connection failure in seconds.
    #[arg(long, help = cfg_help("Reconnect delay on connection failure in seconds", embedded_default(Preset::Balanced).reconnect_delay_secs))]
    pub reconnect_delay_secs: Option<u64>,

    /// Worker router timeout in milliseconds.
    #[arg(long, help = cfg_help("Worker router timeout in milliseconds", embedded_default(Preset::Balanced).worker_router_timeout_ms))]
    pub worker_router_timeout_ms: Option<u64>,

    /// Minimum solver responses before early return (0 = wait for all).
    #[arg(long,
        help = cfg_help(
            "Minimum solver responses before early return (0 = wait for all)",
            embedded_default(Preset::Balanced).worker_router_min_responses,
        ))]
    pub worker_router_min_responses: Option<usize>,

    /// Path to a legacy worker pools TOML config file; overrides the pools defined by
    /// every other config layer. When omitted, ./worker_pools.toml is used if present and
    /// no other layer sets pools.
    #[arg(short, long, env)]
    pub worker_pools_config: Option<PathBuf>,

    /// Path to blocklist TOML config file. Components listed here are excluded from the
    /// Tycho stream.
    #[arg(long, env)]
    pub blocklist_config: Option<PathBuf>,

    /// Gas price staleness threshold in seconds. Health returns 503 when exceeded.
    /// Disabled by default.
    #[arg(long)]
    pub gas_price_stale_threshold_secs: Option<u64>,

    /// Enable partial block (flashblock) updates from the Tycho stream.
    #[arg(long,
        help = cfg_help(
            "Enable partial block (flashblock) updates from the Tycho stream: pool state \
             updates arrive mid-block rather than only at finalization, reducing latency. \
             Only applies to on-chain protocols",
            embedded_default(Preset::Balanced).partial_blocks,
        ))]
    pub partial_blocks: bool,

    /// Enable price guard validation against external price sources.
    /// Disabled by default.
    #[arg(long)]
    pub enable_price_guard: bool,

    /// Port for the Prometheus metrics HTTP server (requires `metrics` feature).
    #[cfg(feature = "metrics")]
    #[arg(long, default_value_t = METRICS_PORT, env)]
    pub metrics_port: u16,
}

impl ServeArgs {
    /// Builds the explicit-overrides config layer from the flags the user actually set;
    /// unset flags stay `None` so the lower config layers supply them.
    ///
    /// A worker pools file passed via `--worker-pools-config` is an explicit override for
    /// the whole `pools` section.
    ///
    /// # Errors
    ///
    /// Fails when the worker pools file cannot be read or parsed.
    pub fn explicit_config(&self) -> anyhow::Result<PartialConfig> {
        let pools = self
            .worker_pools_config
            .as_deref()
            .map(WorkerPoolsConfig::load_from_file)
            .transpose()?
            .map(WorkerPoolsConfig::into_pools);
        Ok(PartialConfig {
            min_tvl: self.min_tvl,
            tvl_buffer_ratio: self.tvl_buffer_ratio,
            min_token_quality: self.min_token_quality,
            traded_n_days_ago: self.traded_n_days_ago,
            gas_refresh_interval_secs: self.gas_refresh_interval_secs,
            reconnect_delay_secs: self.reconnect_delay_secs,
            worker_router_timeout_ms: self.worker_router_timeout_ms,
            worker_router_min_responses: self.worker_router_min_responses,
            partial_blocks: self.partial_blocks.then_some(true),
            // An empty --protocols means "not set"; the lower config layers supply the list.
            protocols: (!self.protocols.is_empty()).then(|| self.protocols.clone()),
            pools,
        })
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn test_arg_parsing() {
        let cli = Cli::try_parse_from(vec![
            "fynd",
            "serve",
            "--chain",
            "Ethereum",
            "--http-host",
            "127.0.0.1",
            "--http-port",
            "8080",
            "--tycho-api-key",
            "test-key",
            "--rpc-url",
            "https://rpc.example.com",
            "--tycho-url",
            "wss://custom.tycho.url",
            "--protocols",
            "uniswap_v2,uniswap_v3",
            "--min-tvl",
            "20.0",
            "--worker-pools-config",
            "new_worker_pools.toml",
        ])
        .expect("parse errored");

        let Commands::Serve(args) = cli.command else {
            panic!("expected Serve command");
        };
        assert_eq!(args.chain, Chain::Ethereum);
        assert_eq!(args.http_host, "127.0.0.1");
        assert_eq!(args.http_port, 8080);
        assert_eq!(args.tycho_api_key, Some("test-key".to_string()));
        assert_eq!(args.rpc_url, Some("https://rpc.example.com".to_string()));
        assert_eq!(args.tycho_url, Some("wss://custom.tycho.url".to_string()));
        assert_eq!(args.protocols, vec!["uniswap_v2", "uniswap_v3"]);
        assert_eq!(args.min_tvl, Some(20.0));
        assert_eq!(args.worker_pools_config, Some(PathBuf::from("new_worker_pools.toml")));
        assert_eq!(args.blocklist_config, None);
    }

    #[test]
    fn test_arg_parsing_defaults() {
        // Clear ambient env vars so the test is deterministic regardless of the shell environment.
        std::env::remove_var("RPC_URL");
        std::env::remove_var("TYCHO_API_KEY");
        std::env::remove_var("TYCHO_URL");
        std::env::remove_var("HTTP_HOST");
        std::env::remove_var("HTTP_PORT");
        std::env::remove_var("CONFIG_FILE");
        std::env::remove_var("PRESET");
        std::env::remove_var("WORKER_POOLS_CONFIG");
        let cli = Cli::try_parse_from(vec!["fynd", "serve"]).expect("parse errored");

        let Commands::Serve(args) = cli.command else {
            panic!("expected Serve command");
        };
        assert_eq!(args.chain, Chain::Ethereum);
        assert_eq!(args.http_host, "0.0.0.0");
        assert_eq!(args.http_port, 3000);
        assert_eq!(args.tycho_api_key, None);
        assert_eq!(args.rpc_url, None);
        assert_eq!(args.tycho_url, None);
        assert!(args.protocols.is_empty());
        // Solver-tuning flags default to None: the config layers supply the values.
        assert_eq!(args.config_file, None);
        assert_eq!(args.preset, Preset::Balanced);
        assert_eq!(args.min_tvl, None);
        assert_eq!(args.tvl_buffer_ratio, None);
        assert_eq!(args.gas_refresh_interval_secs, None);
        assert_eq!(args.reconnect_delay_secs, None);
        assert_eq!(args.worker_router_timeout_ms, None);
        assert_eq!(args.worker_router_min_responses, None);
        assert_eq!(args.worker_pools_config, None);
        assert_eq!(args.blocklist_config, None);
        assert!(!args.partial_blocks);
        #[cfg(feature = "metrics")]
        assert_eq!(args.metrics_port, METRICS_PORT);
    }

    #[test]
    fn test_explicit_config_only_set_flags() {
        let cli = Cli::try_parse_from(vec![
            "fynd",
            "serve",
            "--worker-router-timeout-ms",
            "42",
            "--partial-blocks",
            "--protocols",
            "uniswap_v2",
        ])
        .expect("parse errored");
        let Commands::Serve(args) = cli.command else {
            panic!("expected Serve command");
        };
        let overrides = args
            .explicit_config()
            .expect("explicit config errored");
        assert_eq!(overrides.worker_router_timeout_ms, Some(42));
        assert_eq!(overrides.partial_blocks, Some(true));
        assert_eq!(overrides.protocols, Some(vec!["uniswap_v2".to_string()]));
        assert_eq!(overrides.min_tvl, None);
        assert_eq!(overrides.pools, None);
        assert_eq!(overrides.tvl_buffer_ratio, None);
    }

    #[test]
    fn test_openapi_subcommand() {
        let cli = Cli::try_parse_from(vec!["fynd", "openapi"]).expect("parse errored");
        assert_eq!(cli.command, Commands::Openapi);
    }
}
