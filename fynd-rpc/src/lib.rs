//! Fynd RPC - HTTP server for DEX routing
//!
//! This crate provides an HTTP RPC server that exposes Fynd's solving capabilities
//! via REST API endpoints. It builds on [`fynd-core`](https://docs.rs/fynd-core) and adds
//! HTTP infrastructure, worker pool management, and customizable middleware support.
//!
//! # Use Cases
//!
//! - **Turnkey HTTP server**: Use `FyndRpcBuilder` to quickly deploy a routing service
//! - **Custom middleware**: Add authentication, rate limiting, or custom logic to the HTTP layer
//! - **Microservices**: Integrate Fynd as an HTTP microservice in your infrastructure
//!
//! # Main Components
//!
//! - **builder**: `FyndRpcBuilder` for assembling and configuring the HTTP server
//! - **api**: HTTP endpoint handlers (`/v1/solve`, `/v1/health`) and OpenAPI documentation
//! - **config**: Configuration types for pools, algorithms, and blacklists

// Public modules
pub mod api;
pub mod builder;
pub mod config;

// Re-export key RPC types
pub use api::{ApiError, AppState, HealthStatus};
// Re-export price provider trait and types for custom provider implementations
pub use fynd_core::price_guard::provider::{ExternalPrice, PriceProvider, PriceProviderError};
