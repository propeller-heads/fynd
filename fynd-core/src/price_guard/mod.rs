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

pub mod binance_ws;
pub mod config;
pub mod guard;
pub mod hyperliquid;
pub mod provider;
pub mod provider_registry;
pub mod utils;
