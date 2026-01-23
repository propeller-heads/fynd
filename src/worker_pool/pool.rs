//! Worker pool for processing solve tasks.
//!
//! The worker pool manages multiple dedicated OS threads for CPU-bound route finding.
//! Each pool owns multiple SolverWorker instances that compete for tasks from the queue.
//! A pool is configured with a specific algorithm (by name), allowing multiple pools
//! with different algorithms to compete via the OrderManager.
use std::thread::JoinHandle;

use tokio::sync::broadcast;
use tracing::{error, info};

use crate::{
    algorithm::AlgorithmConfig,
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
    types::internal::SolveTask,
    worker_pool::registry::{
        spawn_workers, SpawnWorkersParams, UnknownAlgorithmError, DEFAULT_ALGORITHM,
    },
};

/// Configuration for the worker pool.
#[derive(Debug, Clone)]
pub(crate) struct WorkerPoolConfig {
    /// Human-readable name for this pool (used in logging/metrics).
    /// Can differ from algorithm to distinguish pools with same algorithm but different configs.
    pub name: String,
    /// Algorithm name for this pool (e.g., "most_liquid").
    /// Use `worker_pool::list_algorithms()` to see available options.
    pub algorithm: String,
    /// Number of worker threads.
    pub num_workers: usize,
    /// Configuration for the algorithm used by each worker.
    pub algorithm_config: AlgorithmConfig,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_ALGORITHM.to_string(),
            algorithm: DEFAULT_ALGORITHM.to_string(),
            num_workers: num_cpus::get(),
            algorithm_config: AlgorithmConfig::default(),
        }
    }
}

/// A pool of worker threads for processing solve tasks.
///
/// Each pool is dedicated to a specific algorithm. Workers in the pool
/// compete for tasks from the shared queue.
pub(crate) struct WorkerPool {
    /// Human-readable name for this pool.
    name: String,
    /// Algorithm name for this pool.
    algorithm: String,
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
    /// * `event_rx` - Broadcast receiver for market events (workers subscribe to this)
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm name in config is not registered.
    pub fn spawn(
        config: WorkerPoolConfig,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: SharedMarketDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
    ) -> Result<Self, UnknownAlgorithmError> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let name = config.name.clone();
        let algorithm = config.algorithm.clone();

        // Spawn workers via the algorithm registry
        let params = SpawnWorkersParams {
            algorithm: config.algorithm,
            num_workers: config.num_workers,
            algorithm_config: config.algorithm_config,
            task_rx,
            market_data,
            event_rx,
            shutdown_tx: shutdown_tx.clone(),
        };
        let workers = spawn_workers(params)?;

        info!(
            name = %name,
            algorithm = %algorithm,
            num_workers = workers.len(),
            "worker pool spawned"
        );

        Ok(Self { name, algorithm, workers, shutdown_tx })
    }

    /// Returns the pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the algorithm name for this pool.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the number of workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Shuts down all workers and waits for them to finish.
    pub fn shutdown(self) {
        info!(name = %self.name, "shutting down worker pool");

        // Send shutdown signal
        let _ = self.shutdown_tx.send(());

        // Wait for all workers to finish
        for (i, handle) in self.workers.into_iter().enumerate() {
            if let Err(e) = handle.join() {
                error!(
                    name = %self.name,
                    worker_id = i,
                    "worker thread panicked: {:?}",
                    e
                );
            }
        }

        info!(name = %self.name, "worker pool shut down");
    }
}

/// Builder for WorkerPool with a fluent API.
pub(crate) struct WorkerPoolBuilder {
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

    /// Sets the algorithm by name.
    ///
    /// Use `worker_pool::list_algorithms()` to see available options.
    pub fn algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.config.algorithm = algorithm.into();
        self
    }

    /// Sets the algorithm configuration.
    pub fn algorithm_config(mut self, config: AlgorithmConfig) -> Self {
        self.config.algorithm_config = config;
        self
    }

    /// Sets the number of worker threads.
    pub fn num_workers(mut self, n: usize) -> Self {
        self.config.num_workers = n;
        self
    }

    /// Builds and spawns the worker pool.
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm name is not registered.
    pub fn build(
        self,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: SharedMarketDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
    ) -> Result<WorkerPool, UnknownAlgorithmError> {
        WorkerPool::spawn(self.config, task_rx, market_data, event_rx)
    }
}

impl Default for WorkerPoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}
