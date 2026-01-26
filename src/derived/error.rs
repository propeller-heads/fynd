//! Error types for derived data computations.

use super::computation::ComputationId;

/// Errors that can occur during derived data computation.
#[derive(Debug, thiserror::Error)]
pub enum ComputationError {
    /// A required dependency has not been computed yet.
    #[error("missing dependency: {0}")]
    MissingDependency(ComputationId),

    /// Dependency data has been computed but is invalid.
    #[error("invalid data: {dependency} - {reason}")]
    InvalidDependencyData {
        /// What entity has invalid data (e.g., "spot_prices", "simulation_states").
        dependency: ComputationId,
        /// Description of the validation failure.
        reason: String,
    },

    /// Type mismatch when retrieving dependency output.
    #[error("type mismatch for computation {0}")]
    TypeMismatch(ComputationId),

    /// Computation exceeded its timeout.
    #[error("computation timed out after {elapsed_ms}ms")]
    Timeout {
        /// Time elapsed before timeout.
        elapsed_ms: u64,
    },

    /// No valid data could be computed (e.g., no path to gas token for price).
    #[error("no valid result: {reason}")]
    NoValidResult {
        /// Description of why no result could be computed.
        reason: String,
    },

    /// Simulation failed during computation.
    #[error("simulation failed: {0}")]
    SimulationFailed(String),

    /// Internal error during computation.
    #[error("internal error: {0}")]
    Internal(String),

    /// Invalid configuration provided.
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
}
