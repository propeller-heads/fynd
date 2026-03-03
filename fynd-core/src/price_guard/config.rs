//! Configuration for the PriceGuard.

/// Configuration for external price validation.
#[derive(Debug, Clone)]
pub struct PriceGuardConfig {
    /// Maximum allowed deviation below external price, in basis points (1 bip = 0.01%).
    /// Solutions where `amount_out` is more than this below the external expectation are rejected.
    /// Default: 300 (3%).
    lower_tolerance_bps: u32,

    /// Maximum allowed deviation above external price, in basis points (1 bip = 0.01%).
    /// Solutions where `amount_out` exceeds the external expectation by more than this are
    /// rejected (likely stale/incorrect external price or simulation bug).
    /// Default: 10_000 (100%, i.e., up to double the expected price).
    upper_tolerance_bps: u32,

    /// If `true`, solutions pass through when external prices are unavailable.
    /// If `false`, solutions are rejected when prices cannot be verified.
    /// Default: `false`.
    allow_on_provider_error: bool,

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
            enabled: true,
        }
    }
}

impl PriceGuardConfig {
    pub fn lower_tolerance_bps(&self) -> u32 {
        self.lower_tolerance_bps
    }

    pub fn upper_tolerance_bps(&self) -> u32 {
        self.upper_tolerance_bps
    }

    pub fn allow_on_provider_error(&self) -> bool {
        self.allow_on_provider_error
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn with_lower_tolerance_bps(mut self, bps: u32) -> Self {
        self.lower_tolerance_bps = bps;
        self
    }

    pub fn with_upper_tolerance_bps(mut self, bps: u32) -> Self {
        self.upper_tolerance_bps = bps;
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
