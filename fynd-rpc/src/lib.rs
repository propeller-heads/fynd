#![deny(missing_docs)]
//! HTTP RPC server for the [Fynd](https://fynd.xyz) DEX router.
//!
//! Wraps [fynd-core](fynd_core) with Actix Web to expose swap-routing as a REST service.
//! Use [`FyndRPCBuilder`](builder::FyndRPCBuilder) to assemble and start the server.
//!
//! [`RouteConfigurator`](api::RouteConfigurator) and the handlers put actix-web types in this
//! crate's public API, so an actix-web major bump is a breaking change for `fynd-rpc`; the
//! [`actix_web`] crate is re-exported here so embedders can rely on the same type identity.
//!
//! For documentation and configuration guides see **<https://docs.fynd.xyz/>**.
//! For the full API reference see **<https://docs.fynd.xyz/reference/api>**.
//!
//! ## Endpoints
//!
//! | Endpoint | Description |
//! |---|---|
//! | `POST /v1/quote` | Request an optimal swap route |
//! | `GET /v1/health` | Data freshness and solver readiness |
//! | `GET /v1/info` | Static instance metadata (chain ID, contract addresses) |
//!
//! ## Embedding and overriding endpoints
//!
//! Binaries that embed `fynd-rpc` can replace one endpoint while keeping the rest via
//! [`FyndRPCBuilder::configure_routes`](builder::FyndRPCBuilder::configure_routes). Overrides can
//! only add or shadow routes under `/v1`, and shadowing a default route does not change the
//! OpenAPI spec served at `/docs/`:
//!
//! ```text
//! builder.configure_routes(|scope, _state| {
//!     scope.route("/quote", web::post().to(my_quote))
//! });
//! ```
//!
//! See [`FyndRPCBuilder::configure_routes`](builder::FyndRPCBuilder::configure_routes) for a
//! compiling version of this example.
//!
//! A custom handler can reuse the stock request/response plumbing —
//! [`api::handlers::validate_quote_request`], [`api::handlers::log_quote_outcome`], and
//! [`api::handlers::quote_response`] — and the solving stages
//! [`fynd_core::WorkerPoolRouter::solve`], [`fynd_core::encode_quotes`], and
//! [`fynd_core::finalize_quote`] that the default `/v1/quote` handler is composed of.

/// HTTP endpoint handlers, OpenAPI docs, and shared application state.
pub mod api;
/// Server builder and runner.
pub mod builder;
/// TOML-based worker pool configuration and server defaults.
pub mod config;
/// Protocol discovery via the Tycho RPC.
pub mod protocols;

// Re-export parse_chain so tools that depend on fynd-rpc (not fynd-core) can use it.
use std::path::Path;

/// Re-exported: [`RouteConfigurator`](api::RouteConfigurator) and the handlers use actix-web
/// types; an actix-web major bump is a breaking change for this crate.
pub use actix_web;
pub use fynd_core::types::parse_chain;

/// Installs the process-global custom-chain registry from a `chains.yaml` file.
///
/// Call once at startup before resolving any chain name. No-op-safe to omit for built-in chains.
pub fn init_chain_registry_from_file(path: &Path) -> Result<(), String> {
    use tycho_simulation::tycho_common::models::chain_config::{
        init_chain_registry, ChainConfigRegistry,
    };
    let path = path
        .to_str()
        .ok_or_else(|| "chains-config path is not valid UTF-8".to_string())?;
    let registry = ChainConfigRegistry::from_yaml_file(path).map_err(|e| e.to_string())?;
    init_chain_registry(registry).map_err(|_| "chain registry already initialized".to_string())
}
