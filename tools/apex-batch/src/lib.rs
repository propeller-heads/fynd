//! Offline APEX batch-clearing surplus analysis.
//!
//! Joins two recordings of the same blocks — hindsight's block-batch capture ([`snapshot`]: the
//! orders, their limits, and Fynd's per-order counterfactual) and a `record-market` recording
//! (the pool state) — replays each block through APEX under a matrix of configurations
//! ([`runner`]), and reports how much surplus batch clearing delivers over solving each order
//! alone ([`analysis`]).
//!
//! The two modules the results hinge on are [`scaling`] (APEX's 18-decimal contract, hardened to
//! decline rather than panic) and [`adapter`] (Tycho pools presented to APEX, with the price
//! direction and precision conversions that are easy to get silently backwards).
//!
//! Everything here is offline: no chain, no Tycho, no Fynd. Both inputs are files.

pub mod adapter;
pub mod analysis;
pub mod runner;
pub mod scaling;
pub mod snapshot;
