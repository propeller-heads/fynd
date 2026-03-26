//! Rust client for the [Fynd](https://fynd.exchange) DEX router.
//!
//! `fynd-client` lets you request swap quotes, build signable transaction payloads, and
//! broadcast signed orders through the Fynd RPC API — all from a single typed interface.
//!
//! # Workflow
//!
//! A complete swap runs in three steps: **quote → approve → sign and execute**.
//! See `clients/rust/examples/swap_erc20.rs` for a full walkthrough, or run it against a
//! local Fynd instance with `./scripts/run-example.sh swap_erc20`.
//!
//! # Constructing a client
//!
//! Use [`FyndClientBuilder`] to configure and build a [`FyndClient`]:
//!
//! ```rust,no_run
//! # use fynd_client::FyndClientBuilder;
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
//! For quote-only use (no on-chain transactions), skip the RPC connection with
//! [`FyndClientBuilder::build_quote_only`]:
//!
//! ```rust,no_run
//! # use fynd_client::FyndClientBuilder;
//! let client = FyndClientBuilder::new("https://rpc.fynd.exchange", "https://rpc.fynd.exchange")
//!     .build_quote_only()?;
//! ```
//!
//! # Requesting a quote
//!
//! ```rust,no_run
//! # use fynd_client::{FyndClientBuilder, Order, OrderSide, QuoteOptions, QuoteParams};
//! # use bytes::Bytes;
//! # use num_bigint::BigUint;
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let client = FyndClientBuilder::new("https://rpc.fynd.exchange", "https://rpc.fynd.exchange")
//! #     .build_quote_only()?;
//! // WETH → USDC on mainnet (Vitalik's address as sender).
//! let weth = Bytes::copy_from_slice(
//!     alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").as_slice(),
//! );
//! let usdc = Bytes::copy_from_slice(
//!     alloy::primitives::address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").as_slice(),
//! );
//! let sender = Bytes::copy_from_slice(
//!     alloy::primitives::address!("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045").as_slice(),
//! );
//!
//! let quote = client
//!     .quote(QuoteParams::new(
//!         Order::new(
//!             weth,
//!             usdc,
//!             BigUint::from(1_000_000_000_000_000_000u64), // 1 WETH (18 decimals)
//!             OrderSide::Sell,
//!             sender,
//!             None,
//!         ),
//!         QuoteOptions::default(),
//!     ))
//!     .await?;
//!
//! println!("amount out: {}", quote.amount_out());
//! # Ok(()) }
//! ```

pub use client::{
    ApprovalParams, ExecutionOptions, FyndClient, FyndClientBuilder, RetryConfig, SigningHints,
    StorageOverrides,
};
pub use error::{ErrorCode, FyndError};
pub use signing::{
    ApprovalPayload, ExecutionReceipt, FyndPayload, MinedTx, SettledOrder, SignedApproval,
    SignedSwap, SwapPayload, TxReceipt,
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
