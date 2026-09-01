//! Fynd CLI - DeFi routing service
//!
//! A command-line application that runs an HTTP RPC server for finding optimal
//! swap routes across multiple DeFi protocols. Uses [`fynd-rpc`] for the HTTP server
//! and [`fynd-core`] for the routing algorithms.
//!
//! # Usage
//!
//! ```bash
//! # All on-chain protocols are fetched from Tycho RPC by default:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz
//!
//! # Combine all on-chain protocols with specific RFQ protocols:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols all_onchain,rfq:bebop
//!
//! # Or specify protocols explicitly:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols uniswap_v2,uniswap_v3
//!
//! # Opt a protocol into streaming its exclusive pools:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols all_onchain,exclusive:ekubo_v3
//!
//! # Serve FermiSwap from the Titan pAMM price level stream instead of simulating it in the EVM:
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --protocols all_onchain,exclude:vm:fermiswap,pricelevelstream:fermiswap
//! ```
//!
//! `--rpc-url` defaults to a chain-specific public endpoint. For production, provide a dedicated
//! one:
//!
//! ```bash
//! fynd serve --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
//!            --rpc-url https://your-rpc-provider.com/v1/your_key
//! ```
//!
//! See `fynd --help` for all available options.
use anyhow::anyhow;
use clap::Parser;
use fynd::{
    cli::{Cli, Commands},
    commands,
    serve::run_solver,
};
use fynd_core::AlgorithmRegistry;
use tracing::error;

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Openapi => {
            let spec = fynd_rpc::api::openapi_spec();
            // Safety: OpenAPI spec serialization only fails on non-string map keys,
            // which utoipa never produces.
            let json = serde_json::to_string_pretty(&spec).expect("spec serialization cannot fail");
            println!("{json}");
            Ok(())
        }
        Commands::Serve(serve_args) => {
            // Log the failure before returning it: the fmt layer writes to stdout, whereas a
            // `main` that returns `Err` prints only to stderr, so log pipelines that follow stdout
            // show a run that stops mid-startup with no reason. Returning the error too keeps the
            // exit code.
            run_solver(*serve_args, AlgorithmRegistry::new()).map_err(|e| {
                error!(error = %e, "Fynd exited with an error");
                anyhow!("{}", e)
            })?;
            Ok(())
        }
        Commands::DeriveConnectorTokens(args) => tokio::runtime::Runtime::new()
            .expect("failed to create tokio runtime")
            .block_on(commands::derive_connector_tokens::run(*args)),
    }
}
