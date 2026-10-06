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
    /// How long after the best candidates were encoded a new round of encoding retries may
    /// start. `Duration::ZERO` turns retrying off.
    encoding_retry_budget: Duration,
}

impl Default for WorkerPoolRouterConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(1000),
            min_responses: 1, // Return as soon as one solver responds
            encoding_retry_budget: defaults::ENCODING_RETRY_BUDGET,
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

    /// Returns how long after the best candidates were encoded a retry round may start.
    pub fn encoding_retry_budget(&self) -> Duration {
        self.encoding_retry_budget
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

    /// Sets how long after the best candidates were encoded a retry round may start.
    pub fn with_encoding_retry_budget(mut self, budget: Duration) -> Self {
        self.encoding_retry_budget = budget;
        self
    }
}
