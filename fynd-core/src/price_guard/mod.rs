//! Price guard: validate solver outputs against external price sources.
//!
//! This module provides the infrastructure for external price validation:
//!
//! - **[`price_guard::provider`](crate::price_guard::provider)**:
//!   [PriceProvider](crate::price_guard::provider::PriceProvider) trait and supporting types
//! - **[`price_guard::provider_registry`](crate::price_guard::provider_registry)**:
//!   [PriceProviderRegistry](crate::price_guard::provider_registry::PriceProviderRegistry) for
//!   managing multiple providers concurrently
//! - **[`price_guard::utils`](crate::price_guard::utils)**: Shared utilities for symbol
//!   normalization, token resolution, staleness checks, and amount computation
//! - **[`price_guard::config`](crate::price_guard::config)**:
//!   [PriceGuardConfig](crate::price_guard::config::PriceGuardConfig) for tolerance thresholds and
//!   fail-open behavior

/// Tolerance thresholds and fail-open configuration for the price guard.
pub mod config;
/// `PriceProvider` trait and supporting error types.
pub mod provider;
/// Registry that manages and queries multiple `PriceProvider`s concurrently.
pub mod provider_registry;
/// Shared utilities: symbol normalisation, token resolution, and staleness checks.
pub mod utils;
