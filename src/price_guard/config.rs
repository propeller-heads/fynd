//! Configuration for the PriceGuard.

/// Configuration for external price validation.
#[derive(Debug, Clone)]
pub struct PriceGuardConfig {
    /// Maximum allowed deviation from external price, in basis points (1 bip = 0.01%).
    /// Solutions where `amount_out` is more than this below the external expectation are rejected.
    /// Default: 300 (3%).
    tolerance_bps: u32,

    /// If `true`, solutions pass through when external prices are unavailable.
    /// If `false`, solutions are rejected when prices cannot be verified.
    /// Default: `true`.
    allow_on_provider_error: bool,

    /// Whether the price guard is enabled.
    /// Default: `true`.
    enabled: bool,
}

impl Default for PriceGuardConfig {
    fn default() -> Self {
        Self { tolerance_bps: 300, allow_on_provider_error: true, enabled: true }
    }
}

impl PriceGuardConfig {
    pub fn tolerance_bps(&self) -> u32 {
        self.tolerance_bps
    }

    pub fn allow_on_provider_error(&self) -> bool {
        self.allow_on_provider_error
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn with_tolerance_bps(mut self, bps: u32) -> Self {
        self.tolerance_bps = bps;
        self
    }

    pub fn with_allow_on_provider_error(mut self, allow_on_provider_error: bool) -> Self {
        self.allow_on_provider_error = allow_on_provider_error;
        self
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}
