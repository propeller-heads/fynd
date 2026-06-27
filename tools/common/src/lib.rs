//! Shared building blocks for Fynd tooling (the `audit` benchmark subcommand and Hindsight).
//!
//! - [`aggregator`] — the quote model (`AggregatorClient` trait, `AggregatorQuote`) that lets Fynd
//!   and external aggregators be compared through a single interface.
//! - [`bps`] — basis-point comparison math between two quote amounts.
//! - [`eth_call`] — [`eth_call::EthCallRunner`], which re-executes encoded calldata on-chain via
//!   `eth_simulateV1` (with an `eth_call` fallback) to measure the real output and gas of a swap.
//! - [`fynd`] — [`fynd::FyndAggregator`], a wrapper that turns `(token_in, token_out, amount)` into
//!   a Fynd quote.
//!
//! ERC-20 storage-slot detection lives in the `erc20-overrides` crate and is used internally by
//! [`eth_call::EthCallRunner`].

pub mod aggregator;
pub mod bps;
pub mod eth_call;
pub mod fynd;
