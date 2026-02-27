//! Rust client for the fynd DEX solver.
//!
//! Provides a three-phase interface for executing trades:
//! 1. **Quote** — [`FyndClient::quote`] — get a priced route from the solver
//! 2. **Sign** — [`FyndClient::signable_payload`] — build an unsigned transaction; sign externally
//! 3. **Execute** — [`FyndClient::execute`] — broadcast the signed transaction to the blockchain

pub mod client;
pub mod error;
pub mod execution;
pub mod signing;
pub mod types;
mod wire;

pub use client::FyndClient;
pub use error::FyndClientError;
pub use execution::{ExecutionReceipt, SettledOrder, TransactionHandle};
pub use signing::{
    assemble_signed_order, FyndPayload, SignablePayload, SignedOrder, TurbinePayload,
};
pub use types::{BlockInfo, Order, OrderSide, OrderSolution, Route, Swap};
