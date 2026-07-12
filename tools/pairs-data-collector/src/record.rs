//! Versioned, integer-preserving collector records.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Exact-input direction relative to the configured pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Configured token A to token B.
    AToB,
    /// Configured token B to token A.
    BToA,
}

/// Role of a quote point in a matched observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteRole {
    /// Fixed-grid exact-input point.
    LadderForward,
    /// Reverse quote whose input is the parent's gross output.
    MatchedReverse,
}

/// Collector-visible terminal result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointStatus {
    /// Fynd returned an executable route.
    Success,
    /// Fynd reported that no route exists.
    NoRouteFound,
    /// Fynd reported insufficient liquidity.
    InsufficientLiquidity,
    /// Fynd timed out.
    Timeout,
    /// Fynd was not ready for the targeted state.
    NotReady,
    /// External price validation failed.
    PriceCheckFailed,
    /// The request failed before producing per-order results.
    RequestFailed,
    /// The target block changed while the quote wave was running.
    BlockRace,
    /// Fynd had already advanced past the RPC-observed head.
    MissedState,
    /// Work was not scheduled within the configured budget.
    CapacitySkipped,
    /// A reverse role could not run because its parent failed.
    ReverseSkippedParentFailed,
    /// A future Fynd status not understood by this collector version.
    Unknown,
}

/// Token fields copied into every quote row for self-contained analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Canonical address.
    pub address: String,
    /// Display symbol.
    pub symbol: String,
    /// ERC-20 decimals.
    pub decimals: u8,
}

/// One attempted source or matched reverse quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotePoint {
    /// Schema version.
    pub schema_version: u32,
    /// Deterministic observation identity.
    pub point_id: String,
    /// Collector run UUID.
    pub run_id: String,
    /// Fixed-grid epoch ID.
    pub grid_epoch_id: String,
    /// Configured pair ID.
    pub pair_id: String,
    /// Pair direction.
    pub direction: Direction,
    /// Position in the configured direction ladder.
    pub depth_index: usize,
    /// Source or reverse role.
    pub quote_role: QuoteRole,
    /// Retry attempt number.
    pub attempt_id: u32,
    /// Parent forward point for matched reverses.
    pub parent_point_id: Option<String>,
    /// Ethereum chain ID.
    pub chain_id: u64,
    /// Observed block number.
    pub block_number: u64,
    /// Observed block hash.
    pub block_hash: String,
    /// Block timestamp in Unix seconds.
    pub block_timestamp: u64,
    /// Wall-clock head receipt time in milliseconds.
    pub head_received_at_ms: u64,
    /// Wall-clock request start in milliseconds.
    pub quote_started_at_ms: u64,
    /// Wall-clock request finish in milliseconds.
    pub quote_finished_at_ms: u64,
    /// Fynd batch solve time.
    pub batch_solve_time_ms: Option<u64>,
    /// Local monotonic quote-wave duration.
    pub monotonic_duration_ms: u64,
    /// Input token metadata.
    pub token_in: TokenRecord,
    /// Output token metadata.
    pub token_out: TokenRecord,
    /// Exact input base units.
    pub amount_in: String,
    /// Gross output base units.
    pub amount_out: Option<String>,
    /// Fynd gas-adjusted output base units.
    pub amount_out_net_gas: Option<String>,
    /// Fynd route gas-unit estimate.
    pub gas_estimate: Option<String>,
    /// Effective Fynd gas price in wei.
    pub gas_price: Option<String>,
    /// Fynd price impact in basis points.
    pub price_impact_bps: Option<i32>,
    /// Forward gross output used as reverse input.
    pub forward_gross_output: Option<String>,
    /// Terminal status.
    pub status: PointStatus,
    /// Boundary or Fynd error detail.
    pub failure_reason: Option<String>,
    /// Full Fynd route JSON for successful pilot points.
    pub route_json: Option<String>,
    /// Fynd package version.
    pub fynd_version: String,
    /// Git SHA supplied at build or runtime.
    pub fynd_git_sha: String,
    /// Full configuration digest.
    pub config_hash: String,
    /// Protocol-universe digest.
    pub protocol_set_hash: String,
    /// Worker configuration digest.
    pub worker_config_hash: String,
}

/// Per-head completeness and timing record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRun {
    /// Schema version.
    pub schema_version: u32,
    /// Collector run UUID.
    pub run_id: String,
    /// Ethereum chain ID.
    pub chain_id: u64,
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: String,
    /// Parent block hash.
    pub parent_hash: String,
    /// Block timestamp.
    pub block_timestamp: u64,
    /// EIP-1559 base fee in wei.
    pub base_fee_per_gas: Option<u64>,
    /// Non-secret RPC endpoint identifier.
    pub rpc_endpoint_id: String,
    /// Head receipt wall time.
    pub head_received_at_ms: u64,
    /// Time Fynd first matched this head.
    pub fynd_ready_at_ms: Option<u64>,
    /// Collection start wall time.
    pub collection_started_at_ms: u64,
    /// Collection finish wall time.
    pub collection_finished_at_ms: u64,
    /// Maximum rows expected from the configured grid.
    pub expected_rows: usize,
    /// Rows actually scheduled.
    pub scheduled_rows: usize,
    /// Successful quote rows.
    pub successful_rows: usize,
    /// Failed or skipped rows.
    pub failed_rows: usize,
    /// Failed rows that describe the market (no route, insufficient liquidity)
    /// rather than a collection failure.
    #[serde(default)]
    pub market_negative_rows: usize,
    /// Block-level terminal status.
    pub status: String,
    /// Full configuration digest.
    pub config_hash: String,
}

/// Append-only canonicality transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockStatusEvent {
    /// Schema version.
    pub schema_version: u32,
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: String,
    /// Canonicality state.
    pub status: CanonicalStatus,
    /// Wall-clock transition time.
    pub status_changed_at_ms: u64,
    /// Head number used for confirmation, if applicable.
    pub canonical_head: Option<u64>,
}

/// Canonicality state from the independent RPC ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalStatus {
    /// First observation of a head.
    Observed,
    /// A competing hash was observed at the same height.
    PotentiallyOrphaned,
    /// Confirmed canonical after the configured depth.
    Canonical,
    /// Replaced by another hash.
    Orphaned,
}

/// Resolved run configuration stored before quote collection starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    /// Schema version.
    pub schema_version: u32,
    /// Collector run UUID.
    pub run_id: String,
    /// Human-readable run name.
    pub run_name: String,
    /// Fixed grid epoch.
    pub grid_epoch_id: String,
    /// Start wall time.
    pub started_at_ms: u64,
    /// Full resolved TOML without secrets.
    pub resolved_config_toml: String,
    /// Full configuration digest.
    pub config_hash: String,
}

/// Tagged record stored in the durable JSON-lines write-ahead log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record_type", content = "record", rename_all = "snake_case")]
pub enum WalRecord {
    /// Run manifest.
    RunManifest(Box<RunManifest>),
    /// Quote point.
    QuotePoint(Box<QuotePoint>),
    /// Per-block completeness record.
    BlockRun(Box<BlockRun>),
    /// Canonicality transition.
    BlockStatusEvent(Box<BlockStatusEvent>),
}

/// Immutable coordinates used to derive a point identity.
pub struct PointIdentity<'a> {
    /// Collector run UUID.
    pub run_id: &'a str,
    /// Target block hash.
    pub block_hash: &'a str,
    /// Configured pair ID.
    pub pair_id: &'a str,
    /// Pair direction.
    pub direction: Direction,
    /// Ladder index.
    pub depth_index: usize,
    /// Source or reverse role.
    pub role: QuoteRole,
}

/// Build a deterministic point identity from its immutable observation coordinates.
pub fn point_id(identity: PointIdentity<'_>) -> String {
    let mut digest = Sha256::new();
    for value in [
        identity.run_id.to_string(),
        identity.block_hash.to_ascii_lowercase(),
        identity.pair_id.to_string(),
        format!("{:?}", identity.direction),
        identity.depth_index.to_string(),
        format!("{:?}", identity.role),
    ] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_identity_is_deterministic_and_role_specific() {
        let make_identity = |role| PointIdentity {
            run_id: "run",
            block_hash: "0xabc",
            pair_id: "weth-usdc",
            direction: Direction::AToB,
            depth_index: 0,
            role,
        };
        let first = point_id(make_identity(QuoteRole::LadderForward));
        let second = point_id(make_identity(QuoteRole::LadderForward));
        let reverse = point_id(make_identity(QuoteRole::MatchedReverse));

        assert_eq!(first, second);
        assert_ne!(first, reverse);
    }
}
