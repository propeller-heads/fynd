//! `fynd-nexus`: holds the RFQ venue connections of one chain, serves the latest prices to Fynd
//! pods, and monitors the health of the chain's components. See the README for the endpoints and
//! the message format.

use clap::Parser;
use fynd_nexus::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .compact()
        .init();
    fynd_nexus::run(Cli::parse()).await
}
