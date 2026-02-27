//! HTTP wire types mirroring `fynd-rpc/src/api/dto.rs` for deserialization.
//!
//! These types are kept internal — callers work with the public types in `types.rs`.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct WireOrder {
    pub token_in: String,
    pub token_out: String,
    #[serde_as(as = "DisplayFromStr")]
    pub amount: BigUint,
    pub side: WireOrderSide,
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireOrderSide {
    Sell,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WireSolutionRequest {
    pub orders: Vec<WireOrder>,
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct WireSolution {
    pub orders: Vec<WireOrderSolution>,
    // Preserved for completeness with the server DTO; not used locally.
    #[allow(dead_code)]
    #[serde_as(as = "DisplayFromStr")]
    pub total_gas_estimate: BigUint,
    #[allow(dead_code)]
    pub solve_time_ms: u64,
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct WireOrderSolution {
    pub order_id: String,
    pub status: WireSolutionStatus,
    pub route: Option<WireRoute>,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_in: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_estimate: BigUint,
    pub price_impact_bps: Option<i32>,
    // Preserved for completeness with the server DTO; not used locally.
    #[allow(dead_code)]
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out_net_gas: BigUint,
    pub block: WireBlockInfo,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WireSolutionStatus {
    Success,
    NoRouteFound,
    InsufficientLiquidity,
    Timeout,
    NotReady,
}

#[derive(Debug, Deserialize)]
pub struct WireBlockInfo {
    pub number: u64,
    pub hash: String,
    pub timestamp: u64,
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct WireRoute {
    pub swaps: Vec<WireSwap>,
}

#[serde_as]
#[derive(Debug, Deserialize)]
pub struct WireSwap {
    pub component_id: String,
    pub protocol: String,
    pub token_in: String,
    pub token_out: String,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_in: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_estimate: BigUint,
}
