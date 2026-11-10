//! The command line and environment of `fynd-nexus`.

use clap::Parser;
use fynd_core::nexus::price_levels::PriceLevelVenue;
use tycho_simulation::evm::tycho_models::Chain;

use crate::{
    market::market_args::MarketArgs, monitor::monitor_args::MonitorArgs, price_levels::VenueArgs,
};

/// The arguments of `fynd-nexus`.
#[derive(Debug, Parser)]
#[command(name = "fynd-nexus", version, about)]
pub struct Cli {
    /// The chain to serve, e.g. ethereum.
    #[arg(long, env = "CHAIN")]
    pub chain: Chain,

    /// Venues to connect to, comma separated. Pass the flag with no value to run
    /// the market alone.
    #[arg(
        long,
        env = "VENUES",
        value_delimiter = ',',
        num_args = 0..,
        default_values_t = PriceLevelVenue::ALL
    )]
    pub venues: Vec<PriceLevelVenue>,

    /// Port of the HTTP API.
    #[arg(long, env = "HTTP_PORT", default_value_t = 3000)]
    pub http_port: u16,

    /// Port of the Prometheus `/metrics` endpoint.
    #[arg(long, env = "METRICS_PORT", default_value_t = 9898)]
    pub metrics_port: u16,

    /// The venues' credentials and endpoints.
    #[command(flatten)]
    pub venue_args: VenueArgs,

    /// The Tycho market's settings. It runs when `--tycho-url` is set.
    #[command(flatten)]
    pub market: MarketArgs,

    /// The health monitor's settings. It runs when `--monitor-components` is set.
    #[command(flatten)]
    pub monitor: MonitorArgs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_without_chain() {
        assert!(Cli::try_parse_from(["fynd-nexus"]).is_err());
    }

    #[test]
    fn test_parse_defaults() {
        let cli = Cli::try_parse_from(["fynd-nexus", "--chain", "base"]).unwrap();

        assert_eq!(cli.chain, Chain::Base);
        assert_eq!(cli.venues, PriceLevelVenue::ALL);
        assert_eq!((cli.http_port, cli.metrics_port), (3000, 9898));
    }

    #[test]
    fn test_parse_unknown_venue() {
        let result = Cli::try_parse_from([
            "fynd-nexus",
            "--chain",
            "ethereum",
            "--venues",
            "bebop,liquorice",
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn test_parse_venue_credential_flags() {
        let cli = Cli::try_parse_from([
            "fynd-nexus",
            "--chain",
            "ethereum",
            "--bebop-key",
            "b",
            "--hashflow-user",
            "u",
            "--hashflow-key",
            "k",
            "--metric-api-key",
            "m",
            "--metric-api-url",
            "https://metric.example",
        ])
        .unwrap();

        assert_eq!(cli.venue_args.bebop.key.as_deref(), Some("b"));
        assert_eq!(cli.venue_args.hashflow.user.as_deref(), Some("u"));
        assert_eq!(cli.venue_args.hashflow.key.as_deref(), Some("k"));
        assert_eq!(cli.venue_args.metric.api_key.as_deref(), Some("m"));
        assert_eq!(cli.venue_args.metric.api_url.as_deref(), Some("https://metric.example"));
    }

    #[test]
    fn test_parse_market_without_venues() {
        let cli = Cli::try_parse_from([
            "fynd-nexus",
            "--chain",
            "ethereum",
            "--venues",
            "--tycho-url",
            "tycho.example",
            "--rpc-url",
            "https://node.example",
        ])
        .unwrap();

        assert!(cli.venues.is_empty());
        assert!(cli.market.is_enabled());
    }

    #[test]
    fn test_parse_market_off_by_default() {
        let cli = Cli::try_parse_from(["fynd-nexus", "--chain", "ethereum"]).unwrap();

        assert!(!cli.market.is_enabled());
    }

    #[test]
    fn test_parse_monitor_on_the_market() {
        let cli = Cli::try_parse_from([
            "fynd-nexus",
            "--chain",
            "ethereum",
            "--venues",
            "--tycho-url",
            "tycho.example",
            "--rpc-url",
            "https://node.example",
            "--monitor-components",
            "--probe-rate",
            "5",
        ])
        .unwrap();

        assert!(cli.monitor.monitor_components);
        assert_eq!(cli.monitor.probe_rate, 5.0);
    }
}
