//! The offline batch runner: replay each captured block through APEX under every configuration.
//!
//! One block is one independent unit of work — its orders, its market state, its price view — so
//! the matrix is embarrassingly parallel and rayon fans it out across `--jobs`. What is *not*
//! independent is market state: the recording is a delta stream, so state at block k is the
//! result of replaying updates 0..=k. [`market_state_at_block`] owns that replay.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::primitives::{Address, U256};
use fynd_test_fixtures::MarketRecording;
use tycho_simulation::{
    protocol::models::ProtocolComponent, tycho_common::simulation::protocol_sim::ProtocolSim,
};

use crate::{adapter::LimitPolicy, snapshot::BlockBatchSnapshot};

/// Which pools the batch may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProtocolSet {
    /// The protocols the latest turbine runs in staging/prod: uniswap v2/v3/v4, sushiswap v2,
    /// pancakeswap v2/v3. Comparable to what a sequencer-embedded APEX would actually see.
    Parity,
    /// Every natively-simulated protocol in the recording. Strictly more liquidity than parity,
    /// and the honest upper bound — VM-backed protocols are absent from both, since their state
    /// cannot be serialized into a recording.
    FullNative,
}

/// Where in the block the batch is placed, which decides the market state it clears against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Position {
    /// State N-1: the block's own swaps have not moved the pools yet. Clean, and the same state
    /// Fynd's captured baseline was solved at.
    Top,
    /// State N: the block's swaps have already moved the pools. Biased *against* the batch — the
    /// pools were moved by the very trades the batch is being asked to clear, so their impact is
    /// counted twice. Reported as a floor, never as the headline.
    #[value(name = "bottom")]
    BiasedBottom,
}

/// One cell of the study's configuration matrix.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub protocol_set: ProtocolSet,
    pub position: Position,
    /// APEX's wall-clock budget for one block. Two are run: an "in-block-ish" budget fixed by the
    /// shadow runs, and a longer exploratory one that shows what the batch is worth without a
    /// latency constraint.
    pub budget_ms: u64,
    /// Stable name for this cell in the output — the join key between the per-order results and
    /// the aggregates.
    pub label: String,
}

impl RunConfig {
    /// The matrix cell's canonical label, e.g. `parity/top/10000ms`.
    pub fn derive_label(protocol_set: ProtocolSet, position: Position, budget_ms: u64) -> String {
        let set = match protocol_set {
            ProtocolSet::Parity => "parity",
            ProtocolSet::FullNative => "full-native",
        };
        let position = match position {
            Position::Top => "top",
            Position::BiasedBottom => "bottom",
        };
        format!("{set}/{position}/{budget_ms}ms")
    }
}

/// Why an order did not enter the batch. Every exclusion is counted under one of these — the
/// study reports coverage explicitly, so a surplus number is always paired with the share of
/// settled volume it was computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExclusionReason {
    /// Fynd's price map has no usable price for one of the order's tokens, so APEX has no
    /// starting price for it.
    TokenUnpriced,
    /// A token declares more than 18 decimals, which APEX's working precision cannot represent.
    DecimalsAbove18,
    /// Scaling the order's amount into 18 decimals overflowed `U256`.
    ScalingOverflow,
    /// The order's pools are outside the run's [`ProtocolSet`] — the parity-vs-full-native axis
    /// made visible per order rather than assumed.
    ProtocolOutsideSet,
    /// Neither an extracted nor a synthetic limit was available, so the order has no floor and
    /// cannot be cleared honestly.
    LimitUnextractable,
}

/// Per-reason exclusion counts for one block under one config.
#[derive(Debug, Clone, Default)]
pub struct ExclusionCounters {
    pub by_reason: HashMap<ExclusionReason, u32>,
    /// Settled notional (in USD, at the block's price view) the exclusions removed. The count
    /// alone understates the gap when the excluded orders are the large ones.
    pub excluded_volume_usd: f64,
}

/// One order's outcome in one batch run: what APEX cleared, against both comparison points.
#[derive(Debug, Clone)]
pub struct OrderResult {
    pub tx_hash: String,
    pub venue: String,
    pub solver: String,
    pub token_in: Address,
    pub token_out: Address,
    /// Output APEX's clearing gave the order, in `token_out` native units. Zero when the order
    /// was in the batch but did not clear — distinct from being excluded, which produces no
    /// `OrderResult` at all.
    pub apex_amount_out: U256,
    /// Fynd's top-of-block quote for the same order, solved alone. The study's baseline.
    pub fynd_amount_out: Option<U256>,
    /// What the order actually received on-chain.
    pub settled_amount_out: U256,
    /// Provenance of the limit the order cleared against, carried through so results can be split
    /// on how much of the surplus rests on a synthetic assumption.
    pub limit_source: Option<crate::snapshot::LimitSource>,
    /// Whether MEV bracketed the settled trade, making `settled_amount_out` an unfair baseline.
    pub sandwiched: bool,
}

/// One block's result under one config.
#[derive(Debug, Clone)]
pub struct BlockResult {
    pub block: u64,
    /// The [`RunConfig::label`] this result belongs to.
    pub label: String,
    pub orders: Vec<OrderResult>,
    pub exclusions: ExclusionCounters,
    /// Wall-clock time APEX spent on this block. The shadow runs read only this.
    pub apex_elapsed: Duration,
    /// Whether APEX hit its deadline and returned a best-so-far result. A deadline-fired result
    /// is partial and its prices are unvalidated, so the analysis reports the rate rather than
    /// pooling those blocks with the converged ones.
    pub deadline_fired: bool,
}

/// The market state at one block: the pools the batch may use, and the components describing
/// them.
///
/// Rebuilt by replaying the recording, so it is owned rather than borrowed — a rayon worker holds
/// its own block's state.
#[derive(Debug, Default)]
pub struct BlockMarketState {
    /// Pool states by Tycho component id.
    pub states: HashMap<String, Arc<dyn ProtocolSim>>,
    /// Component metadata (tokens, protocol id) by component id.
    pub components: HashMap<String, ProtocolComponent>,
}

/// Run every config over every snapshot, fanning blocks out across `jobs` rayon workers.
///
/// Should: replay the recording once per block (see [`market_state_at_block`]), build the pools
/// and orders for each config, call [`solve_block`], and collect the per-block results. Blocks
/// are independent; configs over one block share that block's replayed state, so the replay cost
/// is paid once per block and not once per cell.
pub fn run_matrix(
    _snapshots: &[BlockBatchSnapshot],
    _recording: &MarketRecording,
    _configs: &[RunConfig],
    _limit_policy: LimitPolicy,
    _jobs: usize,
) -> anyhow::Result<Vec<BlockResult>> {
    todo!("fan blocks out across rayon workers, solving every config against each block's state")
}

/// Solve one block under one config.
///
/// Should: build `Pool::Apex` wrappers for the pools inside the config's [`ProtocolSet`], derive
/// the initial prices and market orders, run `apex_solver::run_apex_with_config` with the
/// config's deadline, and project the clearings into [`OrderResult`]s.
///
/// **Must wrap the APEX call in `std::panic::catch_unwind`.** APEX's `validate_result` unwraps a
/// clearing price for every token a pool cleared, and a pool can clear a token that never got a
/// price — see `apex-solver/panic-validate-result.md`. That panics, and an unguarded panic in a
/// rayon worker takes the whole run down after hours of replay. A caught panic is one lost block,
/// counted and reported.
pub fn solve_block(
    _snapshot: &BlockBatchSnapshot,
    _market: &BlockMarketState,
    _config: &RunConfig,
    _limit_policy: LimitPolicy,
) -> anyhow::Result<BlockResult> {
    todo!("build pools and orders, run APEX under catch_unwind, and project the clearings")
}

/// Rebuild the market state as of `block` by replaying the recording's updates.
///
/// Should: fold `recording.updates` in order up to and including the update whose
/// `block_number_or_timestamp` is `block` — the first update is a full snapshot and the rest are
/// deltas, so `states` are overwritten, `new_pairs` inserted, and `removed_pairs` dropped.
/// Returns `None` when the recording does not cover `block`, which is the normal case at the
/// edges (the capture and the recording are separate processes with separate start times) and
/// must skip the block rather than silently clear it against the wrong state.
pub fn market_state_at_block(
    _recording: &MarketRecording,
    _block: u64,
) -> Option<BlockMarketState> {
    todo!("replay updates 0..=k, applying states, new pairs and removals in order")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_config_labels_are_distinct_per_cell() {
        // The label is the join key between per-order results and aggregates, so two cells of the
        // matrix sharing one would silently pool their surplus.
        let mut labels = Vec::new();
        for protocol_set in [ProtocolSet::Parity, ProtocolSet::FullNative] {
            for position in [Position::Top, Position::BiasedBottom] {
                for budget_ms in [10_000u64, 20_000] {
                    labels.push(RunConfig::derive_label(protocol_set, position, budget_ms));
                }
            }
        }
        assert_eq!(labels.len(), 8, "the core matrix is 8 runs per block");
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "duplicate labels: {labels:?}");
        assert!(labels.contains(&"parity/top/10000ms".to_string()));
        assert!(labels.contains(&"full-native/bottom/20000ms".to_string()));
    }
}
