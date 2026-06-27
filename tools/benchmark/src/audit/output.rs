//! Serialisable output types for the `audit` JSON report.

use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct AuditOutput {
    pub(crate) config: AuditConfig,
    pub(crate) results: Vec<TradeResult>,
}

#[derive(Serialize)]
pub(crate) struct AuditConfig {
    pub(crate) fynd_url: String,
    pub(crate) nordstern_url: String,
    pub(crate) chain_id: u64,
    pub(crate) top_pairs: usize,
    pub(crate) amounts_per_pair: usize,
    pub(crate) blocks_used: usize,
    pub(crate) trades_per_block: usize,
    pub(crate) block_stride: usize,
    pub(crate) total_trades: usize,
}

/// Per-participant result — identical shape for Fynd and every external aggregator.
///
/// The first entry in `TradeResult::participants` is always `"fynd"` (the baseline).
/// `amount_out_net_gas` is populated only by Fynd; all other aggregators leave it `null`.
#[derive(Serialize)]
pub(crate) struct ParticipantResult {
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) amount_out: Option<String>,
    /// Net-of-gas output estimate (Fynd only; aggregators set this to `null`).
    pub(crate) amount_out_net_gas: Option<String>,
    /// Reported gas units (Fynd `gas_estimate` or aggregator self-reported gas).
    pub(crate) gas_units: Option<u64>,
    pub(crate) protocols: Vec<String>,
    /// Pool-level route: `[protocol, component_id]` pairs for each swap leg (Fynd only).
    pub(crate) route: Option<Vec<[String; 2]>>,
    /// Parallel sub-routes (aggregators only; `null` for Fynd).
    pub(crate) num_splits: Option<usize>,
    /// Wall-clock time from request dispatch to first byte of response.
    pub(crate) response_time_ms: Option<u64>,
    /// Amount of `token_out` returned by `eth_call` at the quote block.
    pub(crate) eth_call_amount_out: Option<String>,
    /// Difference between `eth_call_amount_out` and `amount_out` in bps.
    pub(crate) eth_call_diff_bps: Option<f64>,
    /// Actual gas consumed by the swap as reported by `eth_simulateV1`.
    pub(crate) eth_call_gas_used: Option<u64>,
    /// Raw bps diff vs Fynd (positive = Fynd better). `null` for the Fynd entry itself.
    pub(crate) raw_diff_bps: Option<f64>,
    /// Gas-adjusted diff using self-reported gas (Fynd `gas_units` and aggregator `gas_units`).
    pub(crate) gas_adjusted_diff_bps_reported: Option<f64>,
    /// Gas-adjusted diff using on-chain gas from `eth_simulateV1`.
    pub(crate) gas_adjusted_diff_bps_onchain: Option<f64>,
}

#[derive(Serialize)]
pub(crate) struct TradeResult {
    pub(crate) block_sample: usize,
    /// Hex-encoded block hash at which all quotes and eth_calls were pinned.
    pub(crate) block_hash: Option<String>,
    pub(crate) pair: String,
    pub(crate) token_in: String,
    pub(crate) token_out: String,
    pub(crate) amount_in: String,
    /// Index into the percentile array (0 = min, amounts_per_pair-1 = max traded amount).
    pub(crate) amount_percentile_idx: usize,
    /// All participants: first entry is `"fynd"`, remainder are aggregators.
    pub(crate) participants: Vec<ParticipantResult>,
}
