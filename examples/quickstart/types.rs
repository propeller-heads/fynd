//! Quickstart-specific types and conversion traits.
//!
//! This module contains types specific to the quickstart example:
//! - Tenderly simulation types
//! - Conversion trait for solver to execution swaps

use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::{models::protocol::ProtocolComponent, Bytes};

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
    pub transaction: TenderlyTransactionDetails,
}

/// Details of a single simulation result.
#[derive(Debug, Deserialize)]
pub struct TenderlySimulationDetails {
    pub status: bool,
    pub gas_used: u64,
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Transaction details from Tenderly simulation.
#[derive(Debug, Deserialize)]
pub struct TenderlyTransactionDetails {
    /// The return value (output) of the transaction as hex string.
    #[serde(default)]
    pub output: Option<String>,
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
