//! On-chain quote simulation and the ERC-20 state overrides it needs.

/// Shared quote/simulation comparison helpers with no RPC dependency.
pub(crate) mod deviation;
/// Decoding of the revert a simulated router call produces.
mod revert;
/// Simulation of encoded Tycho router quotes.
pub mod simulator;
/// Trace-guided discovery of the storage slots a token keys its balances and allowances on.
pub mod token_layout;
/// Request bodies for the simulation RPCs, with the field names of the execution-apis spec.
pub(crate) mod wire;

pub use revert::{decode_error, reason_from_frame};
