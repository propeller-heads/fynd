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
    /// The layer (hop count) at which this cycle was found.
    pub layer: usize,
}

/// A fully evaluated arbitrage cycle with optimized amounts.
#[derive(Debug, Clone)]
pub struct EvaluatedCycle {
    /// The cycle path.
    pub edges: Vec<(Address, Address, String)>,
    /// Optimal input amount found by golden section search.
    pub optimal_amount_in: BigUint,
    /// Output amount at optimal input.
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
