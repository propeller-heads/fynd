//! Tycho Router - A high-performance DEX router built on Tycho.
//!
//! This crate provides a production-ready DEX router that:
//! - Finds optimal swap routes across multiple protocols
//! - Supports multiple concurrent solvers with different algorithms
//! - Maintains real-time market data via Tycho WebSocket
//! - Provides a REST API for solve requests
//!
//! # Architecture
//!
//! The router is organized into several key components:
//!
//! - **API Layer** (`api`): Actix Web HTTP handlers for `/solve`, `/health`, `/metrics`
//! - **Task Queue** (`task_queue`): Bounded queue with backpressure for solve requests
//! - **Worker Pool** (`worker_pool`): Dedicated OS threads for CPU-bound route finding
//! - **Solver** (`solver`): Route finding logic, owns local pool topology copy
//! - **Algorithm** (`algorithm`): Pluggable route-finding algorithms
//! - **Market Data** (`market_data`): Shared state (pools, tokens, gas prices)
//! - **Graph** (`graph`): Graph management for algorithms
//! - **Indexer** (`indexer`): Tycho WebSocket connection, updates market data
//! - **Events** (`events`): Market events broadcast from indexer to solvers
//!
//! # Data Flow
//!
//! ```text
//! HTTP Request -> TaskQueue -> WorkerPool -> Solver -> Algorithm -> Solution
//!                                              ^
//!                                              |
//!                               SharedMarketData (read)
//!                                              ^
//!                                              |
//!                                       TychoIndexer (write)
//! ```
//!
//! # Example Usage
//!
//! ```ignore
//! use tycho_router::{
//!     api::AppState,
//!     indexer::{IndexerBuilder, TychoIndexer},
//!     market_data::SharedMarketData,
//!     task_queue::{TaskQueue, TaskQueueConfig},
//!     worker_pool::{WorkerPool, WorkerPoolConfig},
//! };
//!
//! // Create shared market data
//! let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
//!
//! // Create task queue
//! let task_queue = TaskQueue::new(TaskQueueConfig::default());
//! let (task_handle, task_rx) = task_queue.split();
//!
//! // Create indexer
//! let indexer_config = IndexerBuilder::new()
//!     .tycho_url("wss://tycho.propellerheads.xyz")
//!     .build();
//! let (indexer, event_tx) = TychoIndexer::new(indexer_config, market_data.clone());
//!
//! // Create worker pool
//! let worker_pool = WorkerPool::spawn(
//!     WorkerPoolConfig::default(),
//!     task_rx,
//!     market_data.clone(),
//!     event_tx,
//! );
//!
//! // Start indexer
//! tokio::spawn(indexer.run());
//!
//! // Create app state and start HTTP server
//! let app_state = AppState::new(task_handle, market_data);
//! ```

pub mod algorithm;
pub mod api;
pub mod events;
pub mod graph;
pub mod market_data;
pub mod solver;
pub mod task_queue;
pub mod tycho_feed;
pub mod types;
pub mod worker_pool;

// Re-export commonly used types at crate root
pub use algorithm::{AlgorithmError, MostLiquidAlgorithm};
pub use api::{ApiError, AppState};
pub use events::MarketEvent;
pub use graph::{Edge, GraphManager, Path, PetgraphGraphManager};
pub use market_data::{SharedMarketData, SharedMarketDataRef};
pub use solver::{Solver, SolverConfig};
pub use task_queue::{TaskQueue, TaskQueueConfig, TaskQueueHandle};
pub use tycho_feed::{TychoFeed, TychoFeedBuilder, TychoFeedConfig, TychoFeedError};
pub use types::{
    GasPrice, HealthStatus, Order, OrderSolution, OrderStatus, PoolId, ProtocolSystem, Route,
    Solution, SolutionOptions, SolutionRequest, SolveError, SolveResult, SolveTask, Swap, TaskId,
    Token,
};
pub use worker_pool::{WorkerPool, WorkerPoolBuilder, WorkerPoolConfig};
