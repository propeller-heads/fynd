//! On-chain quote simulation and ERC-20 state override helpers.

/// Shared quote/simulation comparison helpers with no RPC dependency.
pub(crate) mod deviation;
/// ERC-20 balance and allowance storage-slot discovery.
pub mod erc20_slots;
/// Decoding of the revert a simulated router call produces.
mod revert;
/// Simulation of encoded Tycho router quotes.
pub mod simulator;
/// Trace-guided ERC-20 storage-layout discovery used by encoded quote simulation.
mod token_layout;
