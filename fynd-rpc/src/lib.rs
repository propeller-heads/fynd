#![deny(missing_docs)]
//! HTTP RPC server for the [Fynd](https://fynd.xyz) DEX router.
//!
//! Wraps [`fynd-core`] with Actix Web to expose swap-routing as a REST service.
//! Use [`FyndRPCBuilder`](builder::FyndRPCBuilder) to assemble and start the server.
//!
//! For documentation, configuration guides, and API reference see **<https://docs.fynd.xyz/>**.
//!
//! ## Endpoints
//!
//! | Endpoint | Description |
//! |---|---|
//! | `POST /v1/quote` | Request an optimal swap route |
//! | `GET /v1/health` | Data freshness and solver readiness |
//! | `GET /v1/info` | Static instance metadata (chain ID, contract addresses) |

/// HTTP endpoint handlers, OpenAPI docs, and shared application state.
pub mod api;
/// Server builder and runner.
pub mod builder;
/// TOML-based pool configuration and server defaults.
pub mod config;
/// Protocol discovery via the Tycho RPC.
pub mod protocols;
