#![deny(missing_docs)]
//! Pure solving logic for the [Fynd](https://fynd.xyz) DEX router.
//!
//! This crate contains the route-finding algorithms, market-data pipeline, and encoder that
//! powers Fynd. It has **no HTTP dependencies** and can be embedded directly in any application.
//!
//! For documentation, guides, and API reference see **<https://docs.fynd.xyz/>**.
//!
//! # Use cases
//!
//! - **Standalone routing** — embed Fynd's algorithms directly without running an HTTP server.
//! - **Custom algorithms** — implement the [`Algorithm`] trait and register it in an
//!   [`AlgorithmRegistry`], which [`FyndBuilder::with_algorithms`](solver::FyndBuilder) takes.
//! - **HTTP server** — use the [`fynd-rpc`](https://crates.io/crates/fynd-rpc) crate, which wraps
//!   this crate with Actix Web.
//!
//! # Quick start
//!
//! See the [Fynd quickstart](https://docs.fynd.xyz/get-started/quickstart) to run a local
//! instance, or the [custom algorithm guide](https://docs.fynd.xyz/guides/custom-algorithm)
//! to implement your own routing strategy.

/// Route-finding algorithms. Includes [`MostLiquidAlgorithm`],
/// [`algorithm::BellmanFordAlgorithm`], [`PathFrankWolfeAlgorithm`],
/// [`algorithm::WaterFillAlgorithm`], and the pluggable [`Algorithm`] trait.
pub mod algorithm;
/// Derived data computations: spot prices, component depths, and gas prices.
pub mod derived;
/// Encodes solved routes into ABI-encoded on-chain calldata via Tycho's router contracts.
pub mod encoding;
/// Market data feed: Tycho WebSocket integration, gas price fetching, and protocol registry.
pub mod feed;
/// Graph management for algorithms. Provides [`GraphManager`](graph::GraphManager)
/// trait and the reusable [`PetgraphStableDiGraphManager`](graph::PetgraphStableDiGraphManager).
pub mod graph;
/// External price validation for quotes.
pub mod price_guard;
/// Computes the amount out a route delivers when its pAMM legs fall back to Uniswap V3, so the
/// encoder can drop a quote whose fallback pays less than `min_amount_out`.
pub mod propamm_fallback;
/// Re-execute an already-built route against a (possibly newer) market state.
pub mod replay;
/// `eth_call` plumbing shared by the tasks that read contract state.
mod rpc;
/// [`FyndBuilder`](solver::FyndBuilder) assembles the full pipeline and returns a
/// [`Solver`](solver::Solver).
pub mod solver;
/// Core domain types: [`Order`](types::Order), [`Route`](types::Route), [`Quote`](types::Quote),
/// etc.
pub mod types;
/// Multi-threaded solver pool management with pluggable algorithm registry.
pub mod worker_pool;
/// Request orchestration: fans out orders to all solver pools and selects the best result.
pub mod worker_pool_router;

// Re-export commonly used types for convenience
pub use algorithm::{
    registry::AlgorithmRegistry, Algorithm, AlgorithmConfig, AlgorithmError, MostLiquidAlgorithm,
    NoPathReason, PathFrankWolfeAlgorithm,
};
// Required for implementing the Algorithm trait externally
pub use derived::computation::ComputationRequirements;
pub use feed::{events::MarketEvent, market_data::StateLabel};
pub use price_guard::{
    config::PriceGuardConfig,
    provider::{ExternalPrice, PriceProvider, PriceProviderError},
};
pub use replay::{replay_route, ReplayError, RouteReplay};
// `GraphManager`, `Route` and the market data readers take `FxHashMap`/`FxHashSet`.
// Re-exported so an external implementor names the same types without matching our
// `rustc-hash` version itself.
pub use rustc_hash;
pub use solver::{FyndBuilder, PoolConfig, Solver, SolverBuildError, SolverParts, WaitReadyError};
/// Processes ephemeral pending bundles against live Tycho market state. Obtained by calling
/// [`FyndBuilder::build_with_pending`](solver::FyndBuilder::build_with_pending).
pub use tycho_simulation::evm::pending::PendingBlockProcessor;
/// Error type produced by [`PendingBlockProcessor`] when simulating a pending bundle.
pub use tycho_simulation::evm::pending::PendingError;
/// A pending transaction bundle passed to [`PendingBlockProcessor`] for simulation.
pub use tycho_simulation::evm::pending::PendingUpdate;
/// Handle returned by [`FyndBuilder::build_with_step_controller`] that controls when each
/// buffered block is released for decoding. See [`tycho_simulation`] for the full API.
#[cfg(feature = "experimental")]
pub use tycho_simulation::evm::stream::BlockStepController;
/// Implement this trait and register it via
/// [`FyndBuilder::with_pending_indexer`](solver::FyndBuilder::with_pending_indexer)
/// to receive raw transaction deltas during pending-block simulation.
pub use tycho_simulation::tycho_common::traits::TxDeltaIndexer;
pub use types::{
    BlockInfo, ClientFeeParams, ComponentId, EncodingOptions, FeeBreakdown, Order, OrderQuote,
    OrderSide, OrderValidationError, PermitDetails, PermitSingle, Quote, QuoteOptions,
    QuoteRequest, QuoteStatus, Route, RouteValidationError, SingleOrderQuote, SolveError,
    SolveParams, SolveResult, SurplusInfo, Swap, TaskId, Transaction, UserTransferType,
};
pub use worker_pool::{
    pool::{WorkerPool, WorkerPoolBuilder, WorkerPoolConfig},
    registry::UnknownAlgorithmError,
    TaskQueueHandle,
};
pub use worker_pool_router::{
    config::WorkerPoolRouterConfig, encode_quotes, finalize_quote, ExclusiveAccess, LiquidityScope,
    RankedQuotes, SolverPoolHandle, WorkerPoolRouter,
};
