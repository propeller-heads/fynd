//! Worker pool for processing solve tasks.
//!
//! The worker pool manages multiple dedicated OS threads for CPU-bound route finding.
//! Each worker pool owns multiple SolverWorker instances that compete for tasks from the queue.
//! A worker pool is configured with a specific algorithm (by name), allowing multiple worker
//! pools with different algorithms to compete via the WorkerPoolRouter.
//!
//! Worker pools can use either a built-in algorithm (by name via [`WorkerPoolBuilder::algorithm`])
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
        market_data::MarketData,
    },
    graph::EdgeWeightUpdaterWithDerived,
    post_process::registry::PostProcessorFactories,
    types::internal::SolveTask,
    worker_pool::{
        registry::{
            spawn_workers_generic, AlgorithmSpawner, SpawnWorkersParams, SpawnedWorkers,
            UnknownAlgorithmError, DEFAULT_ALGORITHM,
        },
        task_queue::{TaskQueue, TaskQueueConfig, TaskQueueHandle},
    },
    worker_pool_router::LiquidityScope,
};

/// Configuration for the worker pool.
#[derive(Debug)]
pub struct WorkerPoolConfig {
    /// Human-readable name for this worker pool (used in logging/metrics).
    /// Can differ from algorithm to distinguish worker pools with same algorithm but different
    /// configs.
    name: String,
    /// How to spawn workers — either a built-in registry lookup or a custom factory.
    spawner: AlgorithmSpawner,
    /// Number of worker threads.
    num_workers: usize,
    /// Configuration for the algorithm used by each worker.
    algorithm_config: AlgorithmConfig,
    /// Task queue capacity (maximum number of pending tasks).
    task_queue_capacity: usize,
    /// Which liquidity this worker pool's workers ingest.
    liquidity_scope: LiquidityScope,
    /// Protocol systems this worker pool's workers never route through.
    exclude_protocols: Vec<String>,
    /// Protocol systems this worker pool's algorithms never route through, though its
    /// post-processors still see them.
    algorithm_exclude_protocols: Vec<String>,
    /// Factories for the post-processors every worker in this pool builds, in the order they run.
    post_processor_factories: PostProcessorFactories,
}

impl WorkerPoolConfig {
    /// Returns the algorithm name for this worker pool.
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
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            algorithm_exclude_protocols: Vec::new(),
            post_processor_factories: PostProcessorFactories::default(),
        }
    }
}

/// A pool of worker threads for processing solve tasks.
///
/// Each worker pool is dedicated to a specific algorithm. Workers in the pool
/// compete for tasks from the shared queue.
pub struct WorkerPool {
    /// Human-readable name for this worker pool.
    name: String,
    /// Algorithm name for this worker pool.
    algorithm: String,
    /// Handles to worker threads.
    workers: Vec<JoinHandle<()>>,
    /// Whether this pool's algorithm honours the route filter's required tokens.
    supports_required_tokens: bool,
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
    /// * `derived_data` - Shared derived data reference (component depths, token prices)
    /// * `event_rx` - Broadcast receiver for market events (workers subscribe to this)
    /// * `derived_event_rx` - Broadcast receiver for derived data events (resubscribed per worker)
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm name in config is not registered.
    pub fn spawn(
        config: WorkerPoolConfig,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: MarketData,
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
        let liquidity_scope = config.liquidity_scope;
        let exclude_protocols = config.exclude_protocols.clone();
        let algorithm_exclude_protocols = config
            .algorithm_exclude_protocols
            .clone();
        let post_processor_factories = config.post_processor_factories.clone();
        let params = SpawnWorkersParams {
            algorithm: algorithm.clone(),
            pool_name: name.clone(),
            num_workers: config.num_workers,
            algorithm_config: config.algorithm_config,
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope,
            exclude_protocols,
            algorithm_exclude_protocols,
            post_processor_factories,
        };
        let SpawnedWorkers { handles: workers, supports_required_tokens } =
            config.spawner.spawn(params)?;

        info!(
            name = %name,
            algorithm = %algorithm,
            num_workers = workers.len(),
            "worker pool spawned"
        );

        Ok(Self { name, algorithm, workers, supports_required_tokens, shutdown_tx })
    }

    /// Returns the worker pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the algorithm name for this worker pool.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns whether this worker pool's algorithm honours the route filter's required tokens.
    ///
    /// Pass it to [`SolverPoolHandle::with_supports_required_tokens`] when building the handle.
    ///
    /// [`SolverPoolHandle::with_supports_required_tokens`]:
    ///     crate::SolverPoolHandle::with_supports_required_tokens
    pub fn supports_required_tokens(&self) -> bool {
        self.supports_required_tokens
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

    /// Whether this pool will run an algorithm a caller registered.
    #[cfg(test)]
    pub(crate) fn serves_custom_algorithm(&self) -> bool {
        self.config.spawner.is_custom()
    }

    /// Sets the worker pool name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Sets the algorithm by name (built-in registry lookup).
    ///
    /// Available built-in algorithms: `"most_liquid"`, `"bellman_ford"`, `"path_frank_wolfe"`,
    /// and `"water_fill"`.
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
        let name = name.into();
        let spawner =
            Box::new(move |params: SpawnWorkersParams| spawn_workers_generic(params, &factory));
        self.config.spawner = AlgorithmSpawner::Custom { algorithm: name, spawner };
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

    /// Sets which liquidity this pool's workers ingest.
    pub fn liquidity_scope(mut self, scope: LiquidityScope) -> Self {
        self.config.liquidity_scope = scope;
        self
    }

    /// Sets the protocol systems this pool's workers never route through. An entry names a
    /// protocol system exactly, or a whole family when it ends with `:` (`fallback:`).
    pub fn exclude_protocols(mut self, exclude_protocols: Vec<String>) -> Self {
        self.config.exclude_protocols = exclude_protocols;
        self
    }

    /// Sets the protocol systems this pool's algorithms never route through. Post-processors still
    /// see them. Entries match as in [`exclude_protocols`](Self::exclude_protocols).
    pub fn algorithm_exclude_protocols(mut self, algorithm_exclude_protocols: Vec<String>) -> Self {
        self.config.algorithm_exclude_protocols = algorithm_exclude_protocols;
        self
    }

    /// Sets the factories for the post-processors every worker in this pool runs on the route its
    /// algorithm returned, in order. Each worker calls them once, on its own thread.
    pub(crate) fn post_processor_factories(mut self, factories: PostProcessorFactories) -> Self {
        self.config.post_processor_factories = factories;
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
        market_data: MarketData,
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use async_trait::async_trait;

    use super::*;
    use crate::{
        derived::DerivedData,
        feed::events::{EventError, MarketEvent},
        post_process::{PostProcessRequest, PostProcessorRegistry, RoutePostProcessor},
        types::RouteResult,
    };

    struct Noop;

    #[async_trait]
    impl MarketEventHandler for Noop {
        async fn handle_event(&mut self, _event: &MarketEvent) -> Result<(), EventError> {
            Ok(())
        }
    }

    #[async_trait]
    impl RoutePostProcessor for Noop {
        async fn process(
            &self,
            _request: &PostProcessRequest<'_>,
            _route: &RouteResult,
        ) -> Option<RouteResult> {
            None
        }
    }

    /// Each worker builds its own post-processors, so a pool of two calls the factory twice.
    #[test]
    fn test_pool_post_processor_factory_once_per_worker() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let registry = PostProcessorRegistry::new()
            .with_post_processor("count", move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Noop
            })
            .expect("the name is free");
        let factories = registry
            .resolve("pool", &["count".to_string()])
            .expect("the name is registered");
        let (_event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);

        let (pool, _task_handle) = WorkerPoolBuilder::new()
            .num_workers(2)
            .post_processor_factories(factories)
            .build(MarketData::new_shared(), DerivedData::new_shared(), event_rx, derived_event_rx)
            .expect("most_liquid is a built-in algorithm");

        let started = Instant::now();
        while calls.load(Ordering::SeqCst) < 2 && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        pool.shutdown();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
