//! Shared building blocks for Fynd tooling (the `audit` benchmark subcommand and Hindsight).
//!
//! - [`aggregator`] — the quote model (`AggregatorClient` trait, `AggregatorQuote`) that lets Fynd
//!   and external aggregators be compared through a single interface.
//! - [`bps`] — basis-point comparison math between two quote amounts.
//! - [`constants`] — shared address constants (sentinel and zero addresses).
//! - [`swap_simulation`] — [`swap_simulation::EthCallRunner`], which re-executes encoded calldata
//!   on-chain via `eth_simulateV1` (with an `eth_call` fallback) to measure the real output and gas
//!   of a swap.
//! - [`fynd`] — [`fynd::FyndAggregator`], a wrapper that turns `(token_in, token_out, amount)` into
//!   a Fynd quote.
//!
//! ERC-20 storage-slot detection lives in `fynd-core` and is used internally by
//! [`swap_simulation::EthCallRunner`].

pub mod aggregator;
pub mod bps;
pub mod constants;
pub mod fynd;
pub mod swap_simulation;
