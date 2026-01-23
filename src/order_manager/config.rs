//! Configuration for the OrderManager.

use std::time::Duration;

/// Configuration for the OrderManager.
#[derive(Debug, Clone)]
pub(crate) struct OrderManagerConfig {
    /// Default timeout per order (can be overridden per-request).
    pub default_timeout: Duration,
    /// Minimum number of solver responses to wait for before returning early.
    ///
    /// **Behavior:**
    /// - If `0`: Wait for ALL solvers to respond (or hit the timeout).
    /// - If `> 0`: Return as soon as we have `min_responses` solutions, even if the timeout hasn't
    ///   been reached. This enables fast-path responses when some solvers are slower than others.
    ///
    /// The best solution among received responses is still selected.
    pub min_responses: usize,
}

impl Default for OrderManagerConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_millis(100),
            min_responses: 0, // Wait for all by default
        }
    }
}

impl OrderManagerConfig {
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
}
