#![deny(missing_docs)]
//! HTTP RPC server for the [Fynd](https://fynd.xyz) DEX router.
//!
//! Wraps [fynd-core](fynd_core) with Actix Web to expose swap-routing as a REST service.
//! Use [`FyndRPCBuilder`](builder::FyndRPCBuilder) to assemble and start the server.
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

/// HTTP endpoint handlers, OpenAPI docs, and shared application state.
pub mod api;
/// Server builder and runner.
pub mod builder;
/// TOML-based pool configuration and server defaults.
pub mod config;
/// Protocol discovery via the Tycho RPC.
pub mod protocols;

// Re-export key RPC types
pub use api::{ApiError, AppState, HealthStatus};
// Re-export price guard types so users can implement custom providers
// without depending on fynd-core directly.
pub use fynd_core::price_guard::provider::{ExternalPrice, PriceProvider, PriceProviderError};
