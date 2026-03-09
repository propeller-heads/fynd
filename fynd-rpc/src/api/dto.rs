//! Data Transfer Objects (DTOs) for the HTTP API.
//!
//! Types are defined in `fynd-rpc-types` and re-exported here. Conversions
//! between DTO types and `fynd-core` domain types are implemented in
//! `fynd-rpc-types` via `From`/`Into` (enabled by the `core` feature).

pub use fynd_rpc_types::{
    BlockInfo, EncodingOptions, ErrorResponse, HealthStatus, Order, OrderQuote, OrderSide,
    PermitDetails, PermitSingle, Quote, QuoteOptions, QuoteRequest, QuoteStatus, Route,
    SingleOrderQuote, Swap, Transaction, UserTransferType,
};
