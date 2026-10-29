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
