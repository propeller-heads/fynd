//! Scoring logic for competition results.
// Stubs — scorer types will be wired into runner.rs once the snapshot API is available.
#![allow(dead_code)]

use fynd_algo_sdk::{Order, RouteResult};

/// A single order paired with the algorithm's result and the baseline result.
pub struct ScoredOrder {
    /// The order that was solved.
    pub order: Order,
    /// The algorithm's route result, or `None` if it failed.
    pub result: Option<RouteResult>,
    /// The baseline route result, or `None` if the baseline also failed.
    pub baseline: Option<RouteResult>,
}

/// Scores a set of results and returns a single scalar.
pub trait Scorer: Send + Sync {
    /// Computes an aggregate score across all orders.
    fn score(&self, results: &[ScoredOrder]) -> f64;
}

/// Default scorer: sum of `net_amount_out - baseline_net_amount_out` across all orders.
pub struct NetAmountDeltaScorer;

impl Scorer for NetAmountDeltaScorer {
    fn score(&self, _results: &[ScoredOrder]) -> f64 {
        todo!("sum net_amount_out delta across all orders")
    }
}
