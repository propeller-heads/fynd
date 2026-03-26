//! Fynd library — re-exports [`fynd_core`] and [`fynd_rpc`] as a single dependency,
//! letting you build a custom Fynd CLI or embed the solver directly into your own binary.
//!
//! # Usage
//!
//! ```toml
//! [dependencies]
//! fynd = "0.33"
//! ```
//!
//! Then use the re-exported crates directly:
//!
//! ```rust,ignore
//! use fynd::rpc::builder::FyndRPCBuilder;
//! use fynd::core::algorithm::Algorithm;
//! ```

pub use fynd_core as core;
pub use fynd_rpc as rpc;
