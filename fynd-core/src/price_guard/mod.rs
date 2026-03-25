//! Price guard: validate solver outputs against external price sources.
//!
//! This module provides the infrastructure for external price validation:
//!
//! - **guard**: [`PriceGuard`](crate::price_guard::guard::PriceGuard) struct that orchestrates
//!   validation of solver solutions
//! - **provider**: [`PriceProvider`](crate::price_guard::provider::PriceProvider) trait and
//!   supporting types
//! - **provider\_registry**:
//!   [`PriceProviderRegistry`](crate::price_guard::provider_registry::PriceProviderRegistry) for
//!   managing multiple providers concurrently
//! - **utils**: Shared utilities for symbol normalization, token resolution, staleness checks, and
//!   amount computation
//! - **config**: [`PriceGuardConfig`](crate::price_guard::config::PriceGuardConfig) for tolerance
//!   thresholds and fail-open behavior

/// Binance price provider implementation
pub mod binance_ws;
/// Tolerance thresholds and fail-open configuration for the price guard.
pub mod config;
/// Orchestrates validation of solver solutions against external prices.
pub mod guard;
/// Hyperliquid price provider implementation.
pub mod hyperliquid;
/// [`PriceProvider`](crate::price_guard::provider::PriceProvider) trait and supporting error types.
pub mod provider;
/// Registry that manages and queries multiple
/// [`PriceProvider`](crate::price_guard::provider::PriceProvider)s concurrently.
pub mod provider_registry;
/// Shared utilities: symbol normalisation, token resolution, and staleness checks.
pub mod utils;
