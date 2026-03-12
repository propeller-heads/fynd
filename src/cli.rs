use std::path::PathBuf;

use clap::Parser;
use fynd_rpc::config::defaults;

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
}

/// Arguments for the `serve` subcommand.
#[derive(clap::Args, PartialEq, Debug)]
pub struct ServeArgs {
    /// Target chain (e.g. Ethereum)
    #[arg(short, long, default_value = "Ethereum")]
    pub chain: String,

    /// HTTP host (e.g. 0.0.0.0)
    #[arg(long, default_value = defaults::HTTP_HOST, env)]
    pub http_host: String,

    /// HTTP port
    #[arg(long, default_value_t = defaults::HTTP_PORT, env)]
    pub http_port: u16,

    /// Tycho WebSocket URL (default: tycho-beta.propellerheads.xyz)
    #[arg(long, default_value = "localhost:4242", env)]
    pub tycho_url: String,

    /// Tycho API key
    #[arg(long, env)]
    pub tycho_api_key: Option<String>,

    /// Disable TLS for Tycho WebSocket connection
    #[arg(long)]
    pub disable_tls: bool,

    /// Node RPC URL for the target chain. Defaults to a public endpoint if not set.
    #[arg(long, env)]
    pub rpc_url: Option<String>,

    /// List of protocols to index (comma-separated, e.g., uniswap_v2,uniswap_v3).
    /// If omitted, all on-chain protocols are fetched from Tycho RPC.
    /// Use "all_onchain" to fetch all on-chain protocols and combine with explicit entries,
    /// e.g., --protocols all_onchain,rfq:bebop.
    #[arg(short, long, value_delimiter = ',', value_name = "PROTO1,PROTO2")]
    pub protocols: Vec<String>,

    /// Minimum TVL threshold in native token (e.g. ETH). Components below this threshold will be
    /// removed from the market data.
    #[arg(long, default_value_t = defaults::MIN_TVL)]
    pub min_tvl: f64,

    /// TVL buffer multiplier.
    /// Used to avoid fluctuations caused by components hovering around a single threshold.
    /// Default is 1.1 (10% buffer). For example, if the minimum TVL is 10 ETH, then components
    /// that drop below 10 ETH will be removed from the market data and components that exceed 11
    /// ETH will be added.
    #[arg(long, default_value_t = defaults::TVL_BUFFER_MULTIPLIER)]
    pub tvl_buffer_multiplier: f64,

    /// Minimum token quality filter.
    #[arg(long, default_value_t = defaults::MIN_TOKEN_QUALITY)]
    pub min_token_quality: i32,

    /// Only include tokens traded within this many days.
    #[arg(long, default_value_t = defaults::TRADED_N_DAYS_AGO)]
    pub traded_n_days_ago: u64,

    /// Gas price refresh interval in seconds
    #[arg(long, default_value_t = defaults::GAS_REFRESH_INTERVAL_SECS)]
    pub gas_refresh_interval_secs: u64,

    /// Reconnect delay on connection failure in seconds
    #[arg(long, default_value_t = defaults::RECONNECT_DELAY_SECS)]
    pub reconnect_delay_secs: u64,

    /// Worker router timeout in milliseconds
    #[arg(long, default_value_t = defaults::WORKER_ROUTER_TIMEOUT_MS)]
    pub worker_router_timeout_ms: u64,

    /// Minimum solver responses before early return (0 = wait for all)
    #[arg(long, default_value_t = defaults::WORKER_ROUTER_MIN_RESPONSES)]
    pub worker_router_min_responses: usize,

    /// Path to worker pools TOML config file
    #[arg(short, long, env, default_value = "worker_pools.toml")]
    pub worker_pools_config: PathBuf,

    /// Path to blacklist TOML config file (optional)
    #[arg(long, env, default_value = "blacklist.toml")]
    pub blacklist_config: Option<PathBuf>,
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
        assert_eq!(args.chain, "Ethereum");
        assert_eq!(args.http_host, "127.0.0.1");
        assert_eq!(args.http_port, 8080);
        assert_eq!(args.tycho_api_key, Some("test-key".to_string()));
        assert_eq!(args.rpc_url, Some("https://rpc.example.com".to_string()));
        assert_eq!(args.tycho_url, "wss://custom.tycho.url");
        assert_eq!(args.protocols, vec!["uniswap_v2", "uniswap_v3"]);
        assert_eq!(args.min_tvl, 20.0);
        assert_eq!(args.worker_pools_config, PathBuf::from("new_worker_pools.toml"));
    }

    #[test]
    fn test_arg_parsing_defaults() {
        // Clear ambient env var so the test is deterministic
        std::env::remove_var("RPC_URL");
        let cli = Cli::try_parse_from(vec!["fynd", "serve"]).expect("parse errored");

        let Commands::Serve(args) = cli.command else {
            panic!("expected Serve command");
        };
        assert_eq!(args.chain, "Ethereum");
        assert_eq!(args.http_host, "0.0.0.0");
        assert_eq!(args.http_port, 3000);
        assert_eq!(args.tycho_api_key, None);
        assert_eq!(args.rpc_url, None);
        assert_eq!(args.tycho_url, "localhost:4242");
        assert!(args.protocols.is_empty());
        assert_eq!(args.min_tvl, 10.0);
        assert_eq!(args.tvl_buffer_multiplier, 1.1);
        assert_eq!(args.gas_refresh_interval_secs, 30);
        assert_eq!(args.reconnect_delay_secs, 5);
        assert_eq!(args.worker_router_timeout_ms, 100);
        assert_eq!(args.worker_router_min_responses, 0);
    }

    #[test]
    fn test_arg_parsing_default_worker_pools() {
        let cli = Cli::try_parse_from(vec!["fynd", "serve", "--tycho-api-key", "test-key"])
            .expect("parse errored");

        let Commands::Serve(args) = cli.command else {
            panic!("expected Serve command");
        };
        assert_eq!(args.worker_pools_config, PathBuf::from("worker_pools.toml"));
    }

    #[test]
    fn test_openapi_subcommand() {
        let cli = Cli::try_parse_from(vec!["fynd", "openapi"]).expect("parse errored");
        assert_eq!(cli.command, Commands::Openapi);
    }
}
