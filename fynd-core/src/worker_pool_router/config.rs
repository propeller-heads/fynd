//! Configuration for the WorkerPoolRouter.

use std::time::Duration;

use crate::solver::defaults;

/// Configuration for the WorkerPoolRouter.
#[must_use]
#[derive(Debug, Clone)]
pub struct WorkerPoolRouterConfig {
    /// Default timeout per order (can be overridden per-request).
    default_timeout: Duration,
    /// Minimum number of solver responses to wait for before returning early.
    ///
    /// **Behavior:**
    /// - If `0`: Wait for ALL solvers to respond (or hit the timeout).
    /// - If `> 0`: Return as soon as we have `min_responses` solutions, even if the timeout hasn't
    ///   been reached. This enables fast-path responses when some solvers are slower than others.
    ///
    /// The best solution among received responses is still selected.
    min_responses: usize,
    /// How long encoding retries may take, counted from when the best candidates were encoded.
    /// `Duration::ZERO` turns retrying off.
    encoding_retry_budget: Duration,
    /// How far above the on-chain price an RFQ leg may pay, in basis points, before its route is
    /// dropped.
    rfq_leg_tolerance_bps: u32,
}

impl Default for WorkerPoolRouterConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(1000),
            min_responses: 1, // Return as soon as one solver responds
            encoding_retry_budget: defaults::ENCODING_RETRY_BUDGET,
            rfq_leg_tolerance_bps: defaults::RFQ_LEG_TOLERANCE_BPS,
        }
    }
}

impl WorkerPoolRouterConfig {
    /// Returns the default timeout per order.
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Returns the minimum number of solver responses to wait for.
    pub fn min_responses(&self) -> usize {
        self.min_responses
    }

    /// Returns how long encoding retries may take.
    pub fn encoding_retry_budget(&self) -> Duration {
        self.encoding_retry_budget
    }

    /// Returns the RFQ leg tolerance, in basis points.
    pub fn rfq_leg_tolerance_bps(&self) -> u32 {
        self.rfq_leg_tolerance_bps
    }

    /// Creates a new config with the specified timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets the minimum number of responses to wait for.
    pub fn with_min_responses(mut self, min: usize) -> Self {
        self.min_responses = min;
        self
    }

    /// Sets how long encoding retries may take.
    pub fn with_encoding_retry_budget(mut self, budget: Duration) -> Self {
        self.encoding_retry_budget = budget;
        self
    }

    /// Sets the RFQ leg tolerance, in basis points.
    pub fn with_rfq_leg_tolerance_bps(mut self, bps: u32) -> Self {
        self.rfq_leg_tolerance_bps = bps;
        self
    }
}
