//! Tycho Solver - A high-performance DEX solver built on Tycho.
//!
//! This crate provides a production-ready DEX solver that:
//! - Finds optimal swap routes across multiple protocols
//! - Supports multiple concurrent solver pools with different algorithms
//! - Maintains real-time market data via Tycho WebSocket
//! - Provides a REST API for solve requests
//!
//! # Architecture
//!
//! The solver is organized into several key components:
//!
//! - **API Layer** (`api`): Actix Web HTTP handlers for `/solve`, `/health`, `/metrics`
//! - **Order Manager** (`order_manager`): Orchestrates solve requests across multiple solver pools
//! - **Worker Pool** (`worker_pool`): Dedicated OS threads for CPU-bound route finding
//! - **Task Queue** (`task_queue`): Bounded queue with backpressure for solve requests
//! - **Algorithm** (`algorithm`): Pluggable route-finding algorithms
//! - **Market Data** (`market_data`): Shared state (components, tokens, gas prices)
//! - **Graph** (`graph`): Graph management for algorithms
//! - **Tycho Feed** (`tycho_feed`): Tycho WebSocket connection, updates market data
//! - **Events** (`events`): Market events broadcast from feed to solver workers
//!
//! # Data Flow
//!
//! ```text
//! HTTP Request -> OrderManager -> TaskQueue -> WorkerPool -> SolverWorker -> Algorithm -> Solution
//!                                              ^
//!                                              |
//!                               SharedMarketData (read)
//!                                              ^
//!                                              |
//!                                       TychoFeed (write)
//! ```
//!
//! # Example Usage
//!
//! ```ignore
//! use tycho_solver::{parse_chain, PoolConfig, TychoSolverBuilder};
//! use std::collections::HashMap;
//!
//! // Parse chain
//! let chain = parse_chain("Ethereum")?;
//!
//! // Configure solver pools
//! let mut pools = HashMap::new();
//! pools.insert("most_liquid".to_string(), PoolConfig {
//!     algorithm: "most_liquid".to_string(),
//!     num_workers: 4,
//!     min_hops: 1,
//!     max_hops: 3,
//!     timeout_ms: 100,
//!     task_queue_capacity: 1000,
//! });
//!
//! // Build and run solver
//! let solver = TychoSolverBuilder::new(
//!     chain,
//!     pools,
//!     "wss://tycho.propellerheads.xyz".to_string(),
//!     "https://eth.llamarpc.com".to_string(),
//!     vec!["uniswap_v2".to_string(), "uniswap_v3".to_string()],
//! )
//! .build()?;
//!
//! solver.run().await?;
//! ```

// Public modules - exposed for external library users
pub mod algorithm;
pub mod builder;
pub mod config;
pub mod types;

// Internal modules - public for main.rs and internal use, but not re-exported
pub mod api;
pub mod cli;
pub mod derived;
pub mod feed;
pub mod graph;
pub mod order_manager;
pub mod worker_pool;

// Re-export commonly used types at crate root (public API)
pub use algorithm::{AlgorithmError, MostLiquidAlgorithm};
pub use api::{ApiError, AppState};
pub use builder::{parse_chain, TychoSolver, TychoSolverBuilder};
pub use config::{PoolConfig, WorkerPoolsConfig};
pub use derived::{
    ComputationError, ComputationId, ComputationRequirements, DerivedComputation, DerivedData,
    DerivedDataEvent, PoolDepthKey, PoolDepths, ReadinessTracker, SpotPriceKey, SpotPrices,
    TokenGasPriceKey, TokenGasPrices,
};
pub use types::{
    native_token,
    solution::{Order, SolutionOptions, SolutionRequest},
    ComponentId, HealthStatus, OrderSolution, ProtocolSystem, Route, Solution, SolutionStatus,
    SolveError, Swap, UnsupportedChainError,
};
