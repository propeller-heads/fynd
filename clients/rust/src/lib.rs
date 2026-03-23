//! Rust client for the [Fynd](https://fynd.exchange) DEX router.
//!
//! `fynd-client` lets you request swap quotes, build signable transaction payloads, and
//! broadcast signed orders through the Fynd RPC API — all from a single typed interface.
//!
//! # Constructing a client
//!
//! Use [`FyndClientBuilder`] to configure and build a [`FyndClient`]:
//!
//! ```rust,no_run
//! # use fynd_client::{FyndClient, FyndClientBuilder};
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let client = FyndClientBuilder::new(
//!     "https://rpc.fynd.exchange",
//!     "https://mainnet.infura.io/v3/YOUR_KEY",
//! )
//! .build()
//! .await?;
//! # Ok(()) }
//! ```
//!
//! # Minimal quote example
//!
//! ```rust,no_run
//! # use fynd_client::{FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams};
//! # use bytes::Bytes;
//! # use num_bigint::BigUint;
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let client = FyndClientBuilder::new("https://rpc.fynd.exchange", "https://example.com")
//! #     .build().await?;
//! let weth: Bytes = Bytes::copy_from_slice(&[0xC0; 20]); // placeholder
//! let usdc: Bytes = Bytes::copy_from_slice(&[0xA0; 20]); // placeholder
//! let sender: Bytes = Bytes::copy_from_slice(&[0xd8; 20]); // placeholder
//!
//! let order = Order::new(
//!     weth,
//!     usdc,
//!     BigUint::from(1_000_000_000_000_000_000u64), // 1 WETH (18 decimals)
//!     OrderSide::Sell,
//!     sender,
//!     None,
//! );
//!
//! let quote = client
//!     .quote(QuoteParams::new(order, QuoteOptions::default()))
//!     .await?;
//!
//! println!("amount out: {}", quote.amount_out());
//! # Ok(()) }
//! ```

pub use client::{
    ApprovalOptions, ExecutionOptions, FyndClient, FyndClientBuilder, RetryConfig, SigningHints,
    StorageOverrides, SubmitOptions,
};
pub use error::{ErrorCode, FyndError};
pub use signing::{
    ApprovalPayload, ExecutionReceipt, FyndPayload, MinedTx, SettledOrder, SignablePayload,
    SignedApproval, SignedOrder, TxReceipt,
};
pub use types::{
    BackendKind, BlockInfo, ClientFeeParams, EncodingOptions, FeeBreakdown, HealthStatus,
    InstanceInfo, Order, OrderSide, PermitDetails, PermitSingle, Quote, QuoteOptions, QuoteParams,
    QuoteStatus, Route, Swap, Transaction, UserTransferType,
};

mod client;
mod error;
mod mapping;
mod signing;
mod types;
