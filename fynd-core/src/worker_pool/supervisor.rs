//! Worker-session supervision: respawn policy and (from Task 4) the session loop.

use std::time::Duration;

/// Retry policy for respawning a panicked worker.
///
/// Backoff doubles per consecutive failure up to `max_backoff` (the same
/// doubling-with-cap shape as the Rust client's `RetryConfig`). After
/// `max_attempts` consecutive failures the worker gives up. A session that
/// lives at least `stable_session` resets the budget, so spaced transient
/// panics respawn indefinitely while deterministic failures stop fast.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RespawnPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_attempts: u32,
    pub stable_session: Duration,
}

impl Default for RespawnPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            max_attempts: 10,
            stable_session: Duration::from_secs(600),
        }
    }
}

/// What the supervision loop does after a session panicked.
#[derive(Debug, PartialEq)]
pub(crate) enum FailureAction {
    /// Sleep this long, then respawn the worker.
    Retry(Duration),
    /// Stop respawning this worker.
    GiveUp,
}

/// Tracks consecutive session failures against a [`RespawnPolicy`].
pub(crate) struct RespawnState {
    policy: RespawnPolicy,
    consecutive_failures: u32,
    next_backoff: Duration,
}

impl RespawnState {
    pub(crate) fn new(policy: RespawnPolicy) -> Self {
        Self { policy, consecutive_failures: 0, next_backoff: policy.initial_backoff }
    }

    /// Records a panicked session that lived `session_lived` and decides the next action.
    pub(crate) fn on_failure(&mut self, session_lived: Duration) -> FailureAction {
        if session_lived >= self.policy.stable_session {
            self.consecutive_failures = 0;
            self.next_backoff = self.policy.initial_backoff;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.policy.max_attempts {
            return FailureAction::GiveUp;
        }
        let delay = self.next_backoff;
        self.next_backoff = (self.next_backoff * 2).min(self.policy.max_backoff);
        FailureAction::Retry(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fast_policy() -> RespawnPolicy {
        RespawnPolicy {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
            max_attempts: 3,
            stable_session: Duration::from_secs(60),
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut state = RespawnState::new(fast_policy());
        let lived = Duration::from_millis(1);
        assert_eq!(state.on_failure(lived), FailureAction::Retry(Duration::from_millis(100)));
        assert_eq!(state.on_failure(lived), FailureAction::Retry(Duration::from_millis(200)));
        assert_eq!(state.on_failure(lived), FailureAction::GiveUp);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let mut state = RespawnState::new(RespawnPolicy { max_attempts: 1, ..fast_policy() });
        assert_eq!(state.on_failure(Duration::from_millis(1)), FailureAction::GiveUp);
    }

    #[test]
    fn stable_session_resets_the_budget() {
        let mut state = RespawnState::new(fast_policy());
        let rapid = Duration::from_millis(1);
        state.on_failure(rapid);
        state.on_failure(rapid);
        // A session that lived past the stability threshold resets attempts and backoff.
        assert_eq!(
            state.on_failure(Duration::from_secs(61)),
            FailureAction::Retry(Duration::from_millis(100))
        );
    }

    #[test]
    fn backoff_cap_bounds_the_delay() {
        let mut state = RespawnState::new(RespawnPolicy { max_attempts: 10, ..fast_policy() });
        let rapid = Duration::from_millis(1);
        let mut last = Duration::ZERO;
        for _ in 0..5 {
            if let FailureAction::Retry(d) = state.on_failure(rapid) {
                last = d;
            }
        }
        assert_eq!(last, Duration::from_millis(400));
    }
}
