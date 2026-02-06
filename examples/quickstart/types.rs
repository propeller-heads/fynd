//! Quickstart-specific types and conversion traits.
//!
//! This module contains types specific to the quickstart example:
//! - Tenderly simulation types
//! - Conversion trait for solver to execution swaps

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};

// ============================================================================
// Tenderly Simulation Types
// ============================================================================

/// Request payload for Tenderly bundle simulation.
#[derive(Debug, Serialize)]
pub struct TenderlySimulationRequest {
    pub simulations: Vec<TenderlySimulation>,
}

/// A single transaction simulation in a Tenderly bundle.
#[derive(Debug, Serialize)]
pub struct TenderlySimulation {
    pub network_id: String,
    pub from: String,
    pub to: String,
    pub input: String,
    pub value: String,
    pub save: bool,
    pub save_if_fails: bool,
}

/// Response from Tenderly bundle simulation.
#[derive(Debug, Deserialize)]
pub struct TenderlySimulationResponse {
    pub simulation_results: Vec<TenderlySimulationResult>,
}

/// Result for a single transaction in the simulation bundle.
#[derive(Debug, Deserialize)]
pub struct TenderlySimulationResult {
    pub simulation: TenderlySimulationDetails,
}

/// Details of a single simulation result.
#[derive(Debug, Deserialize)]
pub struct TenderlySimulationDetails {
    pub status: bool,
    pub gas_used: u64,
    #[serde(default)]
    pub error_message: Option<String>,
}

// ============================================================================
// Conversion Trait: Solver Swap -> Execution Swap
// ============================================================================

/// Trait for converting solver swaps to execution swaps.
///
/// This trait bridges the gap between `tycho_solver::Swap` (which contains
/// route information) and `tycho_execution::encoding::models::Swap` (which
/// contains execution details including the full ProtocolComponent).
pub trait SwapToExecution {
    /// Converts a solver swap to an execution swap.
    ///
    /// # Arguments
    /// * `component` - The full ProtocolComponent from Tycho (needed for encoding)
    fn to_execution_swap(
        &self,
        component: &ProtocolComponent,
    ) -> tycho_execution::encoding::models::Swap;
}

impl SwapToExecution for tycho_solver::Swap {
    fn to_execution_swap(
        &self,
        component: &ProtocolComponent,
    ) -> tycho_execution::encoding::models::Swap {
        tycho_execution::encoding::models::Swap::new(
            component.clone(),
            Bytes::from(self.token_in.as_ref()),
            Bytes::from(self.token_out.as_ref()),
        )
    }
}

// ============================================================================
// REST API Helper Types
// ============================================================================

/// Request for fetching protocol components via REST API.
#[derive(Debug, Serialize)]
pub struct ProtocolComponentsRequest {
    pub protocol_system: String,
    pub chain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvl_gt: Option<f64>,
    pub pagination: PaginationRequest,
}

/// Pagination parameters for REST API requests.
#[derive(Debug, Serialize)]
pub struct PaginationRequest {
    pub page: usize,
    pub page_size: usize,
}

/// Creates a minimal ProtocolComponent when one isn't found in the cache.
///
/// This is a fallback for when the solver returns a component ID that wasn't
/// fetched via the REST API (e.g., if the component was created after our fetch).
pub fn create_minimal_component(
    component_id: &str,
    protocol: &str,
    chain: Chain,
) -> ProtocolComponent {
    ProtocolComponent {
        id: component_id.to_string(),
        protocol_system: protocol.to_string(),
        protocol_type_name: String::new(),
        chain,
        tokens: Vec::new(),
        contract_addresses: Vec::new(),
        static_attributes: HashMap::new(),
        change: Default::default(),
        creation_tx: Bytes::default(),
        created_at: chrono::NaiveDateTime::default(),
    }
}
