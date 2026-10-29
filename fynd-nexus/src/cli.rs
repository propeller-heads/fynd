//! The command line and environment of `fynd-nexus`.

use clap::Parser;
use fynd_core::nexus::rfq::RfqVenue;
use tycho_simulation::evm::tycho_models::Chain;

use crate::rfq::RfqArgs;

/// The arguments of `fynd-nexus`.
#[derive(Debug, Parser)]
#[command(name = "fynd-nexus", version, about)]
pub struct Cli {
    /// The chain to serve, e.g. ethereum.
    #[arg(long, env = "CHAIN")]
    pub chain: Chain,

    /// Venues to connect to, comma separated.
    #[arg(
        long,
        env = "VENUES",
        value_delimiter = ',',
        default_values_t = RfqVenue::ALL
    )]
    pub venues: Vec<RfqVenue>,

    /// Port of the HTTP API.
    #[arg(long, env = "HTTP_PORT", default_value_t = 3000)]
    pub http_port: u16,

    /// Port of the Prometheus `/metrics` endpoint.
    #[arg(long, env = "METRICS_PORT", default_value_t = 9898)]
    pub metrics_port: u16,

    /// The venues' credentials and endpoints.
    #[command(flatten)]
    pub rfq: RfqArgs,
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
        assert_eq!(cli.venues, RfqVenue::ALL);
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

        assert_eq!(cli.rfq.bebop.key.as_deref(), Some("b"));
        assert_eq!(cli.rfq.hashflow.user.as_deref(), Some("u"));
        assert_eq!(cli.rfq.hashflow.key.as_deref(), Some("k"));
        assert_eq!(cli.rfq.metric.api_key.as_deref(), Some("m"));
        assert_eq!(cli.rfq.metric.api_url.as_deref(), Some("https://metric.example"));
    }
}
