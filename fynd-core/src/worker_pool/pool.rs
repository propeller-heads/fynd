//! Worker pool for processing solve tasks.
//!
//! The worker pool manages multiple dedicated OS threads for CPU-bound route finding.
//! Each pool owns multiple SolverWorker instances that compete for tasks from the queue.
//! A pool is configured with a specific algorithm (by name), allowing multiple pools
//! with different algorithms to compete via the WorkerPoolRouter.
//!
//! Pools can use either a built-in algorithm (by name via [`WorkerPoolBuilder::algorithm`])
//! or a custom [`Algorithm`](crate::algorithm::Algorithm) implementation (via
//! [`WorkerPoolBuilder::with_algorithm`]).
use std::thread::JoinHandle;

use tokio::sync::broadcast;
use tracing::{error, info};

use crate::{
    algorithm::AlgorithmConfig,
    derived::{events::DerivedDataEvent, SharedDerivedDataRef},
    feed::{
        events::{MarketEvent, MarketEventHandler},
        market_data::SharedMarketDataRef,
    },
    graph::EdgeWeightUpdaterWithDerived,
    types::internal::SolveTask,
    worker_pool::{
        registry::{
            AlgorithmSpawner, SpawnWorkersParams, SpawnerHandle, UnknownAlgorithmError,
            DEFAULT_ALGORITHM,
        },
        task_queue::{TaskQueue, TaskQueueConfig, TaskQueueHandle},
    },
};

/// Configuration for the worker pool.
#[derive(Debug)]
pub struct WorkerPoolConfig {
    /// Human-readable name for this pool (used in logging/metrics).
    /// Can differ from algorithm to distinguish pools with same algorithm but different configs.
    name: String,
    /// How to spawn workers — either a built-in registry lookup or a custom factory.
    spawner: AlgorithmSpawner,
    /// Number of worker threads.
    num_workers: usize,
    /// Configuration for the algorithm used by each worker.
    algorithm_config: AlgorithmConfig,
    /// Task queue capacity (maximum number of pending tasks).
    task_queue_capacity: usize,
}

impl WorkerPoolConfig {
    /// Returns the algorithm name for this pool.
    pub fn algorithm_name(&self) -> &str {
        self.spawner.algorithm_name()
    }
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_ALGORITHM.to_string(),
            spawner: AlgorithmSpawner::Registry { algorithm: DEFAULT_ALGORITHM.to_string() },
            num_workers: num_cpus::get(),
            algorithm_config: AlgorithmConfig::default(),
            task_queue_capacity: 1000,
        }
    }
}

/// A pool of worker threads for processing solve tasks.
///
/// Each pool is dedicated to a specific algorithm. Workers in the pool
/// compete for tasks from the shared queue.
pub struct WorkerPool {
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
    /// * `derived_data` - Shared derived data reference (pool depths, token prices)
    /// * `event_rx` - Broadcast receiver for market events (workers subscribe to this)
    /// * `derived_event_rx` - Broadcast receiver for derived data events (resubscribed per worker)
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm name in config is not registered.
    pub fn spawn(
        config: WorkerPoolConfig,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: SharedMarketDataRef,
        derived_data: SharedDerivedDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
        derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    ) -> Result<Self, UnknownAlgorithmError> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let name = config.name.clone();
        let algorithm = config
            .spawner
            .algorithm_name()
            .to_string();

        // Spawn workers
        let params = SpawnWorkersParams {
            algorithm: algorithm.clone(),
            num_workers: config.num_workers,
            algorithm_config: config.algorithm_config,
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx: shutdown_tx.clone(),
        };
        let workers = config.spawner.spawn(params)?;

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
///
/// # Built-in algorithms
///
/// Use [`algorithm`](Self::algorithm) to select a built-in algorithm by name (e.g.,
/// `"most_liquid"`).
///
/// # Custom algorithms
///
/// Use [`with_algorithm`](Self::with_algorithm) to plug in any type implementing
/// [`Algorithm`](crate::algorithm::Algorithm) via a factory closure, bypassing the built-in
/// registry entirely. See the `custom_algorithm` example for a full walkthrough.
#[must_use = "a builder does nothing until .build() is called"]
pub struct WorkerPoolBuilder {
    config: WorkerPoolConfig,
}

impl WorkerPoolBuilder {
    /// Create a builder with default configuration values.
    pub fn new() -> Self {
        Self { config: WorkerPoolConfig::default() }
    }

    /// Sets the pool name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Sets the algorithm by name (built-in registry lookup).
    ///
    /// Available built-in algorithms: `"most_liquid"`.
    pub fn algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.config.spawner = AlgorithmSpawner::Registry { algorithm: algorithm.into() };
        self
    }

    /// Sets a custom algorithm implementation via a factory closure.
    ///
    /// The `factory` is called once per worker thread to create an algorithm instance.
    /// This bypasses the built-in registry, so any type implementing
    /// [`Algorithm`](crate::algorithm::Algorithm) can be used.
    ///
    /// # Example
    ///
    /// ```ignore
    /// builder.with_algorithm("my_algo", |config| MyAlgorithm::new(config))
    /// ```
    pub fn with_algorithm<A, F>(mut self, name: impl Into<String>, factory: F) -> Self
    where
        A: crate::algorithm::Algorithm + 'static,
        A::GraphManager: MarketEventHandler + EdgeWeightUpdaterWithDerived + 'static,
        F: Fn(AlgorithmConfig) -> A + Clone + Send + Sync + 'static,
    {
        self.config.spawner = AlgorithmSpawner::Custom(SpawnerHandle::new(name, factory));
        self
    }

    /// Sets the algorithm from a dynamically loaded [`SpawnerHandle`].
    ///
    /// Use this when loading participant submissions from `.so` files via the competition
    /// harness. The handle carries the type-erased factory that was exported by
    /// `export_algorithm!` inside the participant's shared library.
    #[cfg(feature = "experimental")]
    pub fn with_spawner_handle(mut self, handle: SpawnerHandle) -> Self {
        self.config.spawner = AlgorithmSpawner::Custom(handle);
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

    /// Sets the task queue capacity.
    pub fn task_queue_capacity(mut self, capacity: usize) -> Self {
        self.config.task_queue_capacity = capacity;
        self
    }

    /// Builds and spawns the worker pool.
    ///
    /// Creates an internal task queue and returns both the worker pool and a handle
    /// for enqueueing tasks.
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm name is not registered.
    pub fn build(
        self,
        market_data: SharedMarketDataRef,
        derived_data: SharedDerivedDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
        derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    ) -> Result<(WorkerPool, TaskQueueHandle), UnknownAlgorithmError> {
        // Create task queue internally
        let task_queue =
            TaskQueue::new(TaskQueueConfig { capacity: self.config.task_queue_capacity });
        let (task_handle, task_rx) = task_queue.split();

        // Spawn worker pool
        let pool = WorkerPool::spawn(
            self.config,
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
        )?;

        Ok((pool, task_handle))
    }
}

impl Default for WorkerPoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}
