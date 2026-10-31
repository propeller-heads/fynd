//! The health of one component: its status, and the rules that move it from one status to the
//! next as probes come in.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a component stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Not probed yet, or reset.
    Unknown,
    /// The last probe executed and paid what the component's state predicted.
    Healthy,
    /// Under suspicion: a swap reverted or paid less than predicted, but not often enough to
    /// exclude the component.
    Suspect,
    /// Swaps keep reverting for a reason that is not a token's. Pods exclude it.
    Broken,
    /// Swaps keep paying less than predicted. Pods exclude it.
    Inaccurate,
    /// The component is of a kind that is not executed on chain by the router alone, such as an
    /// RFQ venue, so nothing can be probed.
    NotProbeable,
}

impl HealthStatus {
    /// Every status.
    pub const ALL: [Self; 6] = [
        Self::Unknown,
        Self::Healthy,
        Self::Suspect,
        Self::Broken,
        Self::Inaccurate,
        Self::NotProbeable,
    ];

    /// The name of the status, as in the JSON and the `status` metric label.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Suspect => "suspect",
            Self::Broken => "broken",
            Self::Inaccurate => "inaccurate",
            Self::NotProbeable => "not_probeable",
        }
    }
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// How many bad probes move a component to which status.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthThresholds {
    /// Pool reverts in a row, on different blocks, that make a component [`Broken`].
    ///
    /// [`Broken`]: HealthStatus::Broken
    pub broken_after: u32,
    /// Executed probes in a row with a shortfall over `inaccuracy_bps` that make a component
    /// [`Inaccurate`].
    ///
    /// [`Inaccurate`]: HealthStatus::Inaccurate
    pub inaccurate_after: u32,
    /// How far, in basis points, the output may fall short of the prediction before a probe
    /// counts as a shortfall.
    pub inaccuracy_bps: f64,
}

/// What the last probe that gave a verdict found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastOutcome {
    /// The swap executed.
    Executed,
    /// The swap reverted, and the pool or its venue is to blame.
    PoolReverted,
    /// The swap reverted because a token refused to move.
    TokenReverted,
}

/// What one probe of a component came to, with the block it was run on where there is one.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// The swap executed.
    Executed {
        /// The block of the component's state.
        block: u64,
        /// How far the output is from the prediction, in basis points. Negative is a shortfall.
        deviation_bps: f64,
    },
    /// The swap reverted and the pool or its venue is to blame.
    PoolReverted {
        /// The block of the component's state.
        block: u64,
        /// The decoded revert.
        reason: String,
    },
    /// The swap reverted because a token refused to move. Says nothing about the pool.
    TokenReverted {
        /// The block of the component's state.
        block: u64,
        /// The decoded revert.
        reason: String,
    },
    /// The probe could not run, so it says nothing about the component.
    SetupFailed {
        /// What went wrong.
        reason: String,
    },
    /// The node did not have the block of the component's state.
    BlockUnavailable,
}

impl Observation {
    /// The name of the observation, as in the `outcome` metric label.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Executed { .. } => "executed",
            Self::PoolReverted { .. } => "pool_reverted",
            Self::TokenReverted { .. } => "token_reverted",
            Self::SetupFailed { .. } => "setup_failed",
            Self::BlockUnavailable => "block_unavailable",
        }
    }
}

/// The health record of one component.
///
/// A component that executes and pays about what its state predicted is [`Healthy`]. A pool
/// revert makes it [`Suspect`], and `broken_after` pool reverts in a row, each on a different
/// block, make it [`Broken`]. A shortfall over `inaccuracy_bps` also makes it [`Suspect`], and
/// `inaccurate_after` of them in a row make it [`Inaccurate`]. One executed probe within the
/// threshold makes it [`Healthy`] again. A surplus never counts.
///
/// A probe that could not run, and a block the node no longer has, change no status and no
/// count. A revert that blames a token is recorded but does not count towards [`Broken`]: the
/// token makes every pool that holds it revert.
///
/// [`Healthy`]: HealthStatus::Healthy
/// [`Suspect`]: HealthStatus::Suspect
/// [`Broken`]: HealthStatus::Broken
/// [`Inaccurate`]: HealthStatus::Inaccurate
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// The protocol system of the component.
    pub protocol_system: String,
    /// Where the component stands.
    pub status: HealthStatus,
    /// When the component entered its current status.
    pub status_since: DateTime<Utc>,
    /// When the component joined the probe queue. Later joiners are probed first.
    pub queued_at: DateTime<Utc>,
    /// How many times the component has been probed, including probes that gave no verdict.
    pub probes: u64,
    /// When the component was last probed, or `None` if it has not been since it joined or was
    /// reset.
    pub checked_at: Option<DateTime<Utc>>,
    /// What the last probe that gave a verdict found.
    pub last_outcome: Option<LastOutcome>,
    /// Why the last verdict was bad, such as the decoded revert. `None` after a good verdict.
    pub reason: Option<String>,
    /// How far the last executed probe was from the prediction, in basis points.
    pub deviation_bps: Option<f64>,
    /// The block of the last probe that gave a verdict.
    pub block: Option<u64>,
    /// Pool reverts in a row, each on a different block.
    pub consecutive_pool_reverts: u32,
    /// The block of the last pool revert that counted.
    pub last_pool_revert_block: Option<u64>,
    /// Executed probes in a row with a shortfall over the threshold.
    pub consecutive_shortfalls: u32,
    /// Probes in a row that were not clean: reverts, shortfalls and probes that could not run.
    /// The retry backoff grows with it.
    pub unsettled_checks: u32,
}

impl ComponentHealth {
    /// A component of `protocol_system` that joins the probe queue at `now`.
    pub fn new(protocol_system: String, now: DateTime<Utc>) -> Self {
        Self {
            protocol_system,
            status: HealthStatus::Unknown,
            status_since: now,
            queued_at: now,
            probes: 0,
            checked_at: None,
            last_outcome: None,
            reason: None,
            deviation_bps: None,
            block: None,
            consecutive_pool_reverts: 0,
            last_pool_revert_block: None,
            consecutive_shortfalls: 0,
            unsettled_checks: 0,
        }
    }

    /// Forgets everything probed and puts the component at the front of the probe queue.
    pub fn reset(&mut self, now: DateTime<Utc>) {
        *self = Self::new(std::mem::take(&mut self.protocol_system), now);
    }

    /// Marks the component as one that cannot be probed.
    pub fn mark_not_probeable(&mut self, now: DateTime<Utc>) {
        self.set_status(HealthStatus::NotProbeable, now);
    }

    /// Applies one probe made at `now`.
    pub fn record(
        &mut self,
        observation: Observation,
        thresholds: &HealthThresholds,
        now: DateTime<Utc>,
    ) {
        self.probes += 1;
        self.checked_at = Some(now);
        match observation {
            Observation::Executed { block, deviation_bps } => {
                self.record_executed(block, deviation_bps, thresholds, now);
            }
            Observation::PoolReverted { block, reason } => {
                self.record_revert(LastOutcome::PoolReverted, block, reason);
                if self.last_pool_revert_block != Some(block) {
                    self.consecutive_pool_reverts += 1;
                    self.last_pool_revert_block = Some(block);
                }
                let status = if self.consecutive_pool_reverts >= thresholds.broken_after {
                    HealthStatus::Broken
                } else {
                    HealthStatus::Suspect
                };
                self.set_status(status, now);
            }
            Observation::TokenReverted { block, reason } => {
                self.record_revert(LastOutcome::TokenReverted, block, reason);
            }
            Observation::SetupFailed { .. } | Observation::BlockUnavailable => {
                self.unsettled_checks += 1;
            }
        }
    }

    fn record_executed(
        &mut self,
        block: u64,
        deviation_bps: f64,
        thresholds: &HealthThresholds,
        now: DateTime<Utc>,
    ) {
        self.last_outcome = Some(LastOutcome::Executed);
        self.block = Some(block);
        self.deviation_bps = Some(deviation_bps);
        self.consecutive_pool_reverts = 0;
        self.last_pool_revert_block = None;
        if -deviation_bps > thresholds.inaccuracy_bps {
            self.consecutive_shortfalls += 1;
            self.unsettled_checks += 1;
            self.reason = Some(format!("pays {:.0} bps less than predicted", -deviation_bps));
            let status = if self.consecutive_shortfalls >= thresholds.inaccurate_after {
                HealthStatus::Inaccurate
            } else {
                HealthStatus::Suspect
            };
            self.set_status(status, now);
        } else {
            self.consecutive_shortfalls = 0;
            self.unsettled_checks = 0;
            self.reason = None;
            self.set_status(HealthStatus::Healthy, now);
        }
    }

    fn record_revert(&mut self, outcome: LastOutcome, block: u64, reason: String) {
        self.last_outcome = Some(outcome);
        self.block = Some(block);
        self.reason = Some(reason);
        self.deviation_bps = None;
        self.consecutive_shortfalls = 0;
        self.unsettled_checks += 1;
    }

    fn set_status(&mut self, status: HealthStatus, now: DateTime<Utc>) {
        if self.status != status {
            self.status = status;
            self.status_since = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const THRESHOLDS: HealthThresholds =
        HealthThresholds { broken_after: 2, inaccurate_after: 3, inaccuracy_bps: 100.0 };

    fn executed(block: u64, deviation_bps: f64) -> Observation {
        Observation::Executed { block, deviation_bps }
    }

    fn pool_reverted(block: u64) -> Observation {
        Observation::PoolReverted { block, reason: "PoolEmpty".to_string() }
    }

    fn token_reverted(block: u64) -> Observation {
        Observation::TokenReverted { block, reason: "TransferFailed".to_string() }
    }

    fn setup_failed() -> Observation {
        Observation::SetupFailed { reason: "cannot fund the sender".to_string() }
    }

    fn health_after(observations: Vec<Observation>) -> ComponentHealth {
        let now = Utc::now();
        let mut health = ComponentHealth::new("uniswap_v3".to_string(), now);
        for observation in observations {
            health.record(observation, &THRESHOLDS, now);
        }
        health
    }

    #[rstest]
    #[case::good_probe(vec![executed(1, 0.0)], HealthStatus::Healthy)]
    #[case::first_revert(vec![pool_reverted(1)], HealthStatus::Suspect)]
    #[case::reverts_on_different_blocks(vec![pool_reverted(1), pool_reverted(2)], HealthStatus::Broken)]
    #[case::same_block_repeat(vec![pool_reverted(1), pool_reverted(1)], HealthStatus::Suspect)]
    #[case::good_probe_between_reverts(
        vec![pool_reverted(1), executed(2, 0.0), pool_reverted(3)],
        HealthStatus::Suspect
    )]
    #[case::token_reverts_never_count(
        vec![token_reverted(1), token_reverted(2), token_reverted(3)],
        HealthStatus::Unknown
    )]
    #[case::token_revert_keeps_suspect(
        vec![pool_reverted(1), token_reverted(2)],
        HealthStatus::Suspect
    )]
    #[case::recovery(
        vec![pool_reverted(1), pool_reverted(2), executed(3, -5.0)],
        HealthStatus::Healthy
    )]
    #[case::one_shortfall(vec![executed(1, -150.0)], HealthStatus::Suspect)]
    #[case::shortfalls_in_a_row(
        vec![executed(1, -150.0), executed(2, -150.0), executed(3, -150.0)],
        HealthStatus::Inaccurate
    )]
    #[case::shortfall_streak_broken(
        vec![executed(1, -150.0), executed(2, -150.0), executed(3, -10.0), executed(4, -150.0)],
        HealthStatus::Suspect
    )]
    #[case::shortfall_exactly_at_threshold(vec![executed(1, -100.0)], HealthStatus::Healthy)]
    #[case::surplus_never_counts(
        vec![executed(1, 400.0), executed(2, 400.0), executed(3, 400.0)],
        HealthStatus::Healthy
    )]
    #[case::inaccurate_recovers(
        vec![
            executed(1, -150.0),
            executed(2, -150.0),
            executed(3, -150.0),
            executed(4, 0.0),
        ],
        HealthStatus::Healthy
    )]
    #[case::revert_interrupts_shortfalls(
        vec![executed(1, -150.0), executed(2, -150.0), pool_reverted(3), executed(4, -150.0)],
        HealthStatus::Suspect
    )]
    #[case::setup_failure_changes_nothing(vec![setup_failed()], HealthStatus::Unknown)]
    #[case::setup_failure_keeps_broken(
        vec![pool_reverted(1), pool_reverted(2), setup_failed(), Observation::BlockUnavailable],
        HealthStatus::Broken
    )]
    #[case::block_unavailable_changes_nothing(
        vec![executed(1, 0.0), Observation::BlockUnavailable],
        HealthStatus::Healthy
    )]
    fn test_status_after(#[case] observations: Vec<Observation>, #[case] expected: HealthStatus) {
        assert_eq!(health_after(observations).status, expected);
    }

    #[test]
    fn test_setup_failure_keeps_the_last_verdict() {
        let health = health_after(vec![pool_reverted(7), setup_failed()]);

        assert_eq!(health.last_outcome, Some(LastOutcome::PoolReverted));
        assert_eq!(health.block, Some(7));
        assert_eq!(health.reason.as_deref(), Some("PoolEmpty"));
        assert_eq!(health.consecutive_pool_reverts, 1);
        assert_eq!(health.probes, 2);
        assert_eq!(health.unsettled_checks, 2);
    }

    #[test]
    fn test_good_probe_clears_counts_and_reason() {
        let health = health_after(vec![pool_reverted(1), executed(2, 3.0)]);

        assert_eq!(
            (
                health.consecutive_pool_reverts,
                health.last_pool_revert_block,
                health.unsettled_checks,
                health.reason,
                health.deviation_bps
            ),
            (0, None, 0, None, Some(3.0))
        );
    }

    #[test]
    fn test_status_since_moves_only_with_the_status() {
        let first = Utc::now();
        let later = first + chrono::Duration::seconds(60);
        let mut health = ComponentHealth::new("uniswap_v3".to_string(), first);

        health.record(pool_reverted(1), &THRESHOLDS, first);
        health.record(pool_reverted(2), &THRESHOLDS, later);
        health.record(pool_reverted(3), &THRESHOLDS, later + chrono::Duration::seconds(60));

        assert_eq!((health.status, health.status_since), (HealthStatus::Broken, later));
    }

    #[test]
    fn test_reset_forgets_the_probes_but_not_the_protocol() {
        let now = Utc::now();
        let mut health = health_after(vec![pool_reverted(1), pool_reverted(2)]);

        health.reset(now);

        assert_eq!(health, ComponentHealth::new("uniswap_v3".to_string(), now));
    }
}
