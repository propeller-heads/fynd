//! Worker pool for processing solve tasks.
//!
//! The worker pool manages dedicated OS threads for CPU-bound route finding.
//! Each worker owns a SolverWorker instance and processes tasks from the queue.
//! A pool is configured with a specific algorithm type, allowing multiple
//! pools with different algorithms to compete via the OrderManager.

use std::{
    fmt,
    sync::Arc,
    thread::{self, JoinHandle},
};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{error, info};

use crate::{
    algorithm::MostLiquidAlgorithm,
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
    types::SolveTask,
    worker::{SolverWorker, WorkerConfig},
};

/// Algorithm type for the worker pool.
///
/// Each pool is dedicated to a single algorithm type. The OrderManager
/// can fan out orders to multiple pools with different algorithms and
/// select the best solution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlgorithmType {
    /// Most liquid path algorithm - finds paths through highest liquidity pools.
    #[default]
    MostLiquid,
    // Future algorithm types can be added here:
    // FastHeuristic,
    // SplitRoute,
    // etc.
}

impl fmt::Display for AlgorithmType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlgorithmType::MostLiquid => write!(f, "most_liquid"),
        }
    }
}

/// Configuration for the worker pool.
#[derive(Debug, Clone)]
pub struct WorkerPoolConfig {
    /// Human-readable name for this pool (used in logging/metrics).
    pub name: String,
    /// Algorithm type for this pool.
    pub algorithm_type: AlgorithmType,
    /// Number of worker threads.
    pub num_workers: usize,
    /// Configuration for each solver.
    pub worker_config: WorkerConfig,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            algorithm_type: AlgorithmType::default(),
            num_workers: num_cpus::get(),
            worker_config: WorkerConfig::default(),
        }
    }
}

/// A pool of worker threads for processing solve tasks.
///
/// Each pool is dedicated to a specific algorithm type. Workers in the pool
/// compete for tasks from the shared queue.
pub struct WorkerPool {
    /// Pool name for identification.
    name: String,
    /// Algorithm type for this pool.
    algorithm_type: AlgorithmType,
    /// Handles to worker threads.
    workers: Vec<JoinHandle<()>>,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
}

impl WorkerPool {
    /// Spawns a new worker pool.
    ///
    /// # Arguments
    ///
    /// * `config` - Worker pool configuration
    /// * `task_rx` - Receiver for tasks from the queue
    /// * `market_data` - Shared market data reference
    /// * `event_rx` - Broadcast sender for market events (workers subscribe to this)
    pub fn spawn(
        config: WorkerPoolConfig,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: SharedMarketDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);

        let mut workers = Vec::with_capacity(config.num_workers);
        let pool_name = config.name.clone();
        let algorithm_type = config.algorithm_type;

        // Spawn workers
        for worker_id in 0..config.num_workers {
            let task_rx = task_rx.clone();
            let market_data = Arc::clone(&market_data);
            let event_rx = event_rx.resubscribe();
            let worker_config = config.worker_config.clone();
            let shutdown_rx = shutdown_tx.subscribe();

            let handle = thread::Builder::new()
                .name(format!("{}-worker-{}", pool_name, worker_id))
                .spawn(move || {
                    // Create a tokio runtime for this thread
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to create tokio runtime");

                    rt.block_on(async move {
                        // Create algorithm
                        let algorithm = MostLiquidAlgorithm::with_config(
                            1,
                            worker_config.max_hops,
                            worker_config.timeout.as_millis() as u64,
                        )
                        .expect("invalid algorithm configuration");

                        // Create solver worker
                        let mut worker =
                            SolverWorker::new(market_data, algorithm, worker_config, worker_id);

                        // Initialize solver graph
                        worker.initialize_graph().await;

                        // Run the worker's main loop
                        worker
                            .run(event_rx, task_rx, shutdown_rx)
                            .await;
                    });
                })
                .expect("failed to spawn worker thread");

            workers.push(handle);
        }

        info!(
            pool = %pool_name,
            algorithm = %algorithm_type,
            num_workers = config.num_workers,
            "solver pool spawned"
        );

        Self { name: pool_name, algorithm_type, workers, shutdown_tx }
    }

    /// Returns the pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the algorithm type for this pool.
    pub fn algorithm_type(&self) -> AlgorithmType {
        self.algorithm_type
    }

    /// Returns the number of workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Shuts down all workers and waits for them to finish.
    pub fn shutdown(self) {
        info!(pool = %self.name, "shutting down solver pool");

        // Send shutdown signal
        let _ = self.shutdown_tx.send(());

        // Wait for all workers to finish
        for (i, handle) in self.workers.into_iter().enumerate() {
            if let Err(e) = handle.join() {
                error!(
                    pool = %self.name,
                    worker_id = i,
                    "worker thread panicked: {:?}",
                    e
                );
            }
        }

        info!(pool = %self.name, "worker pool shut down");
    }
}

/// Builder for WorkerPool with a fluent API.
pub struct WorkerPoolBuilder {
    config: WorkerPoolConfig,
}

impl WorkerPoolBuilder {
    pub fn new() -> Self {
        Self { config: WorkerPoolConfig::default() }
    }

    /// Sets the pool name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Sets the algorithm type.
    pub fn algorithm_type(mut self, algorithm_type: AlgorithmType) -> Self {
        self.config.algorithm_type = algorithm_type;
        self
    }

    /// Sets the number of worker threads.
    pub fn num_workers(mut self, n: usize) -> Self {
        self.config.num_workers = n;
        self
    }

    /// Sets the worker configuration.
    pub fn worker_config(mut self, config: WorkerConfig) -> Self {
        self.config.worker_config = config;
        self
    }

    /// Builds and spawns the solver pool.
    pub fn build(
        self,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: SharedMarketDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
    ) -> WorkerPool {
        WorkerPool::spawn(self.config, task_rx, market_data, event_rx)
    }
}

impl Default for WorkerPoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}
