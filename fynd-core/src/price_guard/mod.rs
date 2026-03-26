//! Price guard: validate solver outputs against external price sources.
//!
//! This module provides the infrastructure for external price validation:
//!
//! - **[`guard`]**: [`PriceGuard`](guard::PriceGuard) struct that orchestrates validation of solver
//!   solutions
//! - **[`provider`]**: [`PriceProvider`](provider::PriceProvider) trait and supporting types
//! - **[`provider_registry`]**: [`PriceProviderRegistry`](provider_registry::PriceProviderRegistry)
//!   for managing multiple providers concurrently
//! - **[`utils`]**: Shared utilities for symbol normalization, token resolution, staleness checks,
//!   and amount computation
//! - **[`config`]**: [`PriceGuardConfig`](config::PriceGuardConfig) for tolerance thresholds and
//!   fail-open behavior

/// Tolerance thresholds and fail-open configuration for the price guard.
pub mod config;
/// [`PriceGuard`](guard::PriceGuard) struct that orchestrates validation of solver solutions.
pub mod guard;
/// Hyperliquid price provider implementation.
pub mod hyperliquid;
/// [`PriceProvider`](provider::PriceProvider) trait and supporting error types.
pub mod provider;
/// Registry that manages and queries multiple [`PriceProvider`](provider::PriceProvider)s
/// concurrently.
pub mod provider_registry;
/// Shared utilities: symbol normalisation, token resolution, and staleness checks.
pub mod utils;
