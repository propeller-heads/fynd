//! The command-line arguments of the Tycho market.

use std::fmt;

/// The settings of the Tycho market.
///
/// The market runs when `--tycho-url` is set, and then needs `--rpc-url` too.
#[derive(clap::Args, Clone)]
pub struct MarketArgs {
    /// Tycho URL, such as `tycho-beta.propellerheads.xyz`, without a scheme. Setting it turns the
    /// market on.
    #[arg(long, env = "TYCHO_URL", requires = "rpc_url")]
    pub tycho_url: Option<String>,

    /// Tycho API key.
    #[arg(long, env = "TYCHO_API_KEY", hide_env_values = true)]
    pub tycho_api_key: Option<String>,

    /// Whether to connect to Tycho over TLS.
    #[arg(long, env = "TYCHO_USE_TLS", default_value_t = true, action = clap::ArgAction::Set)]
    pub tycho_use_tls: bool,

    /// Minimum TVL in the chain's native token for a component to be tracked. Defaults to the
    /// chain's low TVL threshold.
    #[arg(long, env = "MIN_TVL")]
    pub min_tvl: Option<f64>,

    /// Node RPC URL, which the market reads gas prices from.
    #[arg(long, env = "RPC_URL", hide_env_values = true)]
    pub rpc_url: Option<String>,
}

impl MarketArgs {
    /// Whether the market is turned on.
    pub fn is_enabled(&self) -> bool {
        self.tycho_url.is_some()
    }
}

impl fmt::Debug for MarketArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarketArgs")
            .field("tycho_url", &self.tycho_url)
            .field("tycho_api_key", &self.tycho_api_key.is_some())
            .field("tycho_use_tls", &self.tycho_use_tls)
            .field("min_tvl", &self.min_tvl)
            .field("rpc_url", &self.rpc_url.is_some())
            .finish()
    }
}
