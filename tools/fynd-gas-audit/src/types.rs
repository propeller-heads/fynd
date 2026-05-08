//! Shared types for the audit pipeline.

use num_bigint::BigUint;
use serde::Serialize;
use serde_with::{serde_as, DisplayFromStr};

/// A trade sampled from the aggregator dataset, ready to quote.
#[derive(Debug, Clone, Serialize)]
pub struct AuditTrade {
    pub token_in: String,  // 0x-prefixed lowercase hex
    pub token_out: String, // 0x-prefixed lowercase hex
    pub amount_in: String, // raw atomic units, decimal string
    pub sender: String,    // 0x-prefixed lowercase hex
}

/// Final outcome for one trade after quote + simulation.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RowStatus {
    /// Both quote and simulation succeeded.
    Success,
    /// Fynd returned no successful route.
    NoQuote,
    /// Quote succeeded but had no encoded transaction (e.g. encoding disabled).
    NoEncoding,
    /// `eth_estimateGas` reverted or failed.
    SimulationReverted,
}

/// One row of the output CSV / aggregate table.
///
/// `error_wei` and `error_eth` are not stored — derive them from `error_gas`
/// and `gas_price_wei` at the point of consumption (see `report::error_eth`).
///
/// `BigUint` fields use `DisplayFromStr` so they serialise as decimal strings
/// in CSV (the default serde representation is a sequence of u32 digits, which
/// the csv crate can't put in a column).
#[serde_as]
#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub gas_estimate: Option<BigUint>, // None if no quote
    pub actual_gas: Option<u64>, // None if quote or simulation failed
    #[serde_as(as = "DisplayFromStr")]
    pub gas_price_wei: BigUint, // constant across the run
    pub error_gas: Option<i128>, // estimate - actual
    pub status: RowStatus,
    pub error_reason: Option<String>,
    /// Number of swaps in the chosen route. None when no route was returned.
    /// `1` = single-hop, `>1` = sequential.
    pub num_swaps: Option<usize>,
    /// Comma-joined list of protocol identifiers used by the route, in order.
    /// E.g. `"uniswap_v3,vm:balancer_v2"`. None when no route was returned.
    pub protocols: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_status_serializes_snake_case() {
        let s = serde_json::to_string(&RowStatus::NoQuote).unwrap();
        assert_eq!(s, "\"no_quote\"");
        let s = serde_json::to_string(&RowStatus::SimulationReverted).unwrap();
        assert_eq!(s, "\"simulation_reverted\"");
    }
}
