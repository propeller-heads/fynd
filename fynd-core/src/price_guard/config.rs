use serde::{Deserialize, Serialize};

/// Configuration for the PriceGuard external price validation.
///
/// Controls tolerance thresholds, fail-open behavior, and whether validation
/// is enabled at all. All fields have sensible defaults via [`Default`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceGuardConfig {
    /// Maximum allowed deviation when `amount_out < expected`, in basis points.
    /// Solutions where the user gets less than expected by more than this are rejected.
    /// Default: 300 (3%).
    lower_tolerance_bps: u32,

    /// Maximum allowed deviation when `amount_out >= expected`, in basis points.
    /// Solutions that exceed external expectations by more than this are rejected
    /// (may indicate a bug in our pricing).
    /// Default: 10_000 (100%).
    upper_tolerance_bps: u32,

    /// Controls behavior when all providers error for reasons unrelated to pricing
    /// (network issues, API down).
    /// `false` (default): reject solutions when no provider can return a price.
    /// `true`: let solutions pass through when no provider can be reached.
    allow_on_provider_error: bool,

    /// Controls behavior when every provider returns `PriceNotFound` for the
    /// requested token pair.
    /// `false` (default): reject solutions for unknown token pairs.
    /// `true`: let solutions pass through when no provider has a price.
    allow_on_token_price_not_found: bool,

    /// Whether the price guard is enabled.
    /// Default: `true`.
    enabled: bool,
}

impl Default for PriceGuardConfig {
    fn default() -> Self {
        Self {
            lower_tolerance_bps: 300,
            upper_tolerance_bps: 10_000,
            allow_on_provider_error: false,
            allow_on_token_price_not_found: false,
            enabled: false,
        }
    }
}

impl PriceGuardConfig {
    /// Maximum allowed negative deviation in basis points (user gets less than expected).
    pub fn lower_tolerance_bps(&self) -> u32 {
        self.lower_tolerance_bps
    }

    /// Maximum allowed positive deviation in basis points (output exceeds expectation).
    pub fn upper_tolerance_bps(&self) -> u32 {
        self.upper_tolerance_bps
    }

    /// Whether solutions pass through when all providers are unreachable.
    pub fn allow_on_provider_error(&self) -> bool {
        self.allow_on_provider_error
    }

    /// Whether solutions pass through when no provider has a price for the pair.
    pub fn allow_on_token_price_not_found(&self) -> bool {
        self.allow_on_token_price_not_found
    }

    /// Whether price-guard validation is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Set the lower tolerance threshold in basis points.
    pub fn with_lower_tolerance_bps(mut self, bps: u32) -> Self {
        self.lower_tolerance_bps = bps;
        self
    }

    /// Set the upper tolerance threshold in basis points.
    pub fn with_upper_tolerance_bps(mut self, bps: u32) -> Self {
        self.upper_tolerance_bps = bps;
        self
    }

    /// Set whether solutions pass through when all providers error.
    pub fn with_allow_on_provider_error(mut self, allow: bool) -> Self {
        self.allow_on_provider_error = allow;
        self
    }

    /// Set whether solutions pass through when no provider has a price.
    pub fn with_allow_on_token_price_not_found(mut self, allow: bool) -> Self {
        self.allow_on_token_price_not_found = allow;
        self
    }

    /// Enable or disable price-guard validation.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}
