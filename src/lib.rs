//! Fynd library — re-exports [`fynd_core`] and [`fynd_rpc`] as a single dependency,
//! letting you build a custom Fynd CLI or embed the solver directly into your own binary.
//!
//! # Usage
//!
//! ```toml
//! [dependencies]
//! fynd = "0.33"
//! ```
//!
//! Then use the re-exported crates directly:
//!
//! ```rust,ignore
//! use fynd::rpc::builder::FyndRPCBuilder;
//! use fynd::core::algorithm::Algorithm;
//! ```
//!
//! To run Fynd's own command line with an algorithm of your own, parse [`cli::Cli`] and hand
//! [`serve::run_solver`] a registry — the binary keeps every flag `fynd serve` has:
//!
//! ```rust,ignore
//! let algorithms = fynd::core::AlgorithmRegistry::new().with_algorithm("mine", Mine::new)?;
//! match fynd::cli::Cli::parse().command {
//!     fynd::cli::Commands::Serve(args) => fynd::serve::run_solver(*args, algorithms)?,
//!     _ => {}
//! }
//! ```
//!
//! To also change what the server serves, use [`serve::run_solver_with`]: it hands you the
//! builder the CLI configured, so an embedder can override a route or rewrite the OpenAPI
//! document without restating a single flag.
//!
//! ```rust,ignore
//! fynd::serve::run_solver_with(*args, algorithms, |builder| {
//!     builder.configure_routes(my_routes::configure)
//! })?;
//! ```

pub use fynd_core as core;
pub use fynd_rpc as rpc;

/// Command-line arguments, so a binary embedding Fynd can parse the same ones.
pub mod cli;
/// Subcommands other than `serve`.
pub mod commands;
/// Running the solver, as `fynd serve` does.
pub mod serve;
