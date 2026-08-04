//! Aggregation over the runner's per-order results — the numbers the study reports.
//!
//! The headline is per-order surplus against **Fynd's top-of-block counterfactual**, not against
//! the settled amount: the question is what batch clearing adds over the best single-order
//! routing we can do, and the settled amount is a third-party solver's routing plus its own
//! margin. The settled comparison is kept alongside because it is the number that is externally
//! checkable, and because a batch that beats Fynd but loses to the settlement is a result worth
//! seeing rather than hiding.
//!
//! Coverage travels with every aggregate. A surplus figure computed over 40% of a block's volume
//! is a different claim from the same figure over 95%, and the plan is explicit that the
//! parity-versus-full-native asymmetry gets reported verbosely rather than averaged away.

use std::collections::HashMap;

use crate::runner::{BlockResult, ExclusionReason};

/// Surplus for one comparison basis, in both absolute and relative terms.
///
/// Both are kept because neither alone is meaningful: dollars answer "is this worth building",
/// basis points answer "is this a real edge or rounding", and a handful of large trades can make
/// the two disagree.
#[derive(Debug, Clone, Default)]
pub struct Surplus {
    /// Signed USD difference, valued at the block's own price view.
    pub usd: f64,
    /// Signed difference in basis points of the compared output.
    pub bps: f64,
    /// Orders this surplus was computed over.
    pub orders: u32,
    /// Share of those orders where the batch came out ahead.
    pub win_rate: f64,
}

/// One block's aggregate under one config.
#[derive(Debug, Clone)]
pub struct BlockSummary {
    pub block: u64,
    pub label: String,
    /// APEX versus Fynd's top-of-block counterfactual — the headline.
    pub vs_fynd: Surplus,
    /// APEX versus what actually settled on-chain — the externally checkable comparison.
    pub vs_settled: Surplus,
    /// Settled notional the batch actually covered, in USD.
    pub cleared_volume_usd: f64,
    /// Settled notional in the block that never entered the batch, in USD.
    pub excluded_volume_usd: f64,
    /// Orders excluded, by reason. Reported per block, not just in the run total, because a
    /// single unpriced high-volume token can dominate one block and no other.
    pub exclusions: HashMap<ExclusionReason, u32>,
    /// Whether APEX ran out of budget on this block. A deadline-fired block's surplus is a lower
    /// bound, so pooling it with converged blocks understates the configuration.
    pub deadline_fired: bool,
}

/// A named slice of the results — one venue, one solver, or one limit provenance.
///
/// The limit split is the load-bearing one: surplus measured against a synthetic floor rests on
/// the 100 bps assumption, and surplus measured against an extracted floor does not. If the two
/// disagree, the assumption is the finding.
#[derive(Debug, Clone)]
pub struct GroupSummary {
    /// The group's value: the venue name, solver name, or `extracted`/`synthetic`.
    pub name: String,
    pub vs_fynd: Surplus,
    pub vs_settled: Surplus,
    /// Settled notional in this group, for weighing a high-surplus group that is tiny.
    pub volume_usd: f64,
}

/// One configuration's aggregate across every block it ran.
#[derive(Debug, Clone)]
pub struct MatrixSummary {
    pub label: String,
    pub blocks: Vec<BlockSummary>,
    /// Totals over every block in this cell.
    pub total: Surplus,
    pub by_venue: Vec<GroupSummary>,
    pub by_solver: Vec<GroupSummary>,
    /// Split by whether the order's limit was extracted or synthetic.
    pub by_limit_source: Vec<GroupSummary>,
    /// Share of the run's settled volume that entered the batch at all — the coverage number
    /// every surplus figure above must be read against.
    pub covered_volume_share: f64,
    /// Share of blocks where APEX hit its deadline.
    pub deadline_fired_share: f64,
}

/// Aggregate the runner's per-block results into one summary per configuration.
///
/// Should: group `results` by [`crate::runner::RunConfig::label`], summarize each block against
/// both comparison bases, roll the blocks up into a cell total, and cut each cell by venue,
/// solver and limit provenance. Sandwiched orders are reported in their own group but kept out of
/// the headline totals — their settled output was moved by MEV, so scoring against it measures
/// the sandwich, not the batch.
pub fn aggregate(_results: &[BlockResult], _prices: &HashMap<String, f64>) -> Vec<MatrixSummary> {
    todo!("group results by config label, summarize per block, then roll up and cut by group")
}

/// Wall-clock percentiles for a shadow run.
///
/// The shadow run exists to answer one question before the matrix is worth running: does APEX
/// finish a block of Ethereum-scale orders inside a block time? The tail matters more than the
/// median — a p50 inside budget with a p99 ten times over is not an in-block solver.
#[derive(Debug, Clone, Default)]
pub struct TimingSummary {
    pub blocks: u32,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Median pool count per block, since runtime is expected to track it — the knob (min TVL,
    /// top-K pools per pair) the study tunes if the tail is too slow.
    pub median_pools: u32,
}

/// Summarize a shadow run's per-block wall-clock into percentiles.
///
/// Should: collect each result's `apex_elapsed`, sort, and read the percentiles off. An empty
/// result set yields a zeroed summary rather than an error — a shadow run over no blocks is a
/// configuration mistake the caller reports, not a computation failure.
pub fn timings(_results: &[BlockResult]) -> TimingSummary {
    todo!("sort the per-block elapsed times and read p50/p90/p99/max off them")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_aggregate_keeps_configs_apart() {
        // Two cells of the matrix must never pool: the whole point of the matrix is that
        // top-of-block and biased-bottom give different answers.
        let summaries = aggregate(&[], &HashMap::new());
        assert!(summaries.is_empty(), "no results means no configurations to summarize");
    }

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_timings_over_no_blocks_is_zeroed() {
        // A shadow run that matched no blocks is a configuration mistake, reported as zero
        // blocks rather than as an error or a fabricated percentile.
        let summary = timings(&[]);
        assert_eq!(summary.blocks, 0);
        assert_eq!(summary.p50_ms, 0.0);
        assert_eq!(summary.max_ms, 0.0);
    }
}
