//! Types for the atomic searcher.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Address;

/// A candidate arbitrage cycle found by the Bellman-Ford scan.
#[derive(Debug, Clone)]
pub struct CycleCandidate {
    /// Path as (from_token, to_token, component_id) edges.
    pub edges: Vec<(Address, Address, String)>,
    /// Amount produced at the end of the cycle (before optimization).
    pub relaxation_amount_out: BigUint,
    /// The layer (hop count) at which this cycle was found (unused, kept for diagnostics).
    pub _layer: usize,
}

/// A fully evaluated arbitrage cycle with optimized amounts.
#[derive(Debug, Clone)]
pub struct EvaluatedCycle {
    /// The cycle path.
    pub edges: Vec<(Address, Address, String)>,
    /// Optimal input amount found by golden section search.
    pub optimal_amount_in: BigUint,
    /// Output amount at optimal input (used by executor for encoding).
    #[allow(dead_code)]
    pub amount_out: BigUint,
    /// Gross profit: amount_out - amount_in.
    pub gross_profit: BigUint,
    /// Total gas cost in source token terms.
    pub gas_cost: BigUint,
    /// Net profit: gross_profit - gas_cost. Can be negative.
    pub net_profit: i128,
    /// Whether this cycle is profitable after gas.
    pub is_profitable: bool,
}

/// Summary of a single block's search results.
#[derive(Debug)]
pub struct BlockSearchResult {
    /// Block number.
    pub block_number: u64,
    /// Number of candidate cycles found by BF.
    pub candidates_found: usize,
    /// Number of cycles that are profitable after gas.
    pub profitable_cycles: usize,
    /// The evaluated cycles (sorted by net profit descending).
    pub cycles: Vec<EvaluatedCycle>,
    /// Time spent searching (milliseconds).
    pub search_time_ms: u64,
}

/// Execution mode for how to handle profitable cycles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Only log results, no on-chain interaction.
    LogOnly,
    /// Encode + simulate via eth_simulate, but don't send a real tx.
    Simulate,
    /// Encode + sign + send via public mempool.
    ExecutePublic,
    /// Encode + sign + send via Flashbots Protect (private mempool).
    ExecuteProtected,
}

impl ExecutionMode {
    pub fn from_str_arg(s: &str) -> Result<Self, String> {
        match s {
            "log-only" => Ok(Self::LogOnly),
            "simulate" => Ok(Self::Simulate),
            "execute-public" => Ok(Self::ExecutePublic),
            "execute-protected" => Ok(Self::ExecuteProtected),
            other => Err(format!(
                "unknown execution mode '{}'. \
                 Valid: log-only, simulate, execute-public, execute-protected",
                other
            )),
        }
    }
}

/// Result of attempting to execute a cycle on-chain.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Transaction hash (hex-encoded, with 0x prefix).
    pub tx_hash: Option<String>,
    /// Whether the execution succeeded.
    pub success: bool,
    /// Gas used by the transaction (if available).
    pub gas_used: Option<u64>,
    /// Which mode was used.
    pub mode: ExecutionMode,
    /// Human-readable summary or error message.
    pub message: String,
}
