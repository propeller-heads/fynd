//! Worker pool registry for spawning workers with different algorithms.
//!
//! This module provides a registry pattern for built-in algorithms, allowing worker pools
//! to be created by algorithm name (string). For custom algorithms, use
//! [`WorkerPoolBuilder::with_algorithm`](super::pool::WorkerPoolBuilder::with_algorithm)
//! which bypasses the registry entirely.
//!
//! # Adding a New Built-in Algorithm
//!
//! 1. Implement the `Algorithm` trait for your algorithm
//! 2. Add a match arm in `AlgorithmSpawner::spawn` that creates your algorithm
//! 3. Add the algorithm name to `AVAILABLE_ALGORITHMS`

use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

pub use spawner_handle::SpawnerHandle;
mod spawner_handle {
    use std::thread::JoinHandle;

    use super::{spawn_workers_generic, AlgorithmConfig, SpawnWorkersParams};

    /// Type-erased algorithm spawner for dynamic loading.
    ///
    /// Wraps a factory closure so that algorithm plugins loaded via `libloading` can
    /// hand back a single heap-allocated object that the harness can call without knowing
    /// the concrete algorithm type.
    pub struct SpawnerHandle {
        inner: Box<dyn Fn(SpawnWorkersParams) -> Vec<JoinHandle<()>> + Send + Sync>,
        name: String,
    }

    impl SpawnerHandle {
        /// Creates a new `SpawnerHandle` from a factory closure.
        ///
        /// The `factory` is called once per worker thread to construct the algorithm instance.
        pub fn new<A, F>(algorithm_name: impl Into<String>, factory: F) -> Self
        where
            A: crate::algorithm::Algorithm + 'static,
            A::GraphManager: crate::feed::events::MarketEventHandler
                + crate::graph::EdgeWeightUpdaterWithDerived,
            F: Fn(AlgorithmConfig) -> A + Clone + Send + Sync + 'static,
        {
            let name = algorithm_name.into();
            Self { name, inner: Box::new(move |params| spawn_workers_generic(params, &factory)) }
        }

        /// Returns the algorithm name associated with this handle.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// Spawns worker threads using the encapsulated factory.
        ///
        /// Consumes the handle; the returned join handles own the worker threads.
        pub(crate) fn spawn(self, params: SpawnWorkersParams) -> Vec<JoinHandle<()>> {
            (self.inner)(params)
        }
    }
}

use tokio::sync::broadcast;
use tracing::info;

use crate::{
    algorithm::{AlgorithmConfig, BellmanFordAlgorithm, MostLiquidAlgorithm},
    derived::{events::DerivedDataEvent, SharedDerivedDataRef},
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
    types::internal::SolveTask,
    worker_pool::worker::SolverWorker,
};

/// List of available built-in algorithm names (for registry-based dispatch).
pub(crate) const AVAILABLE_ALGORITHMS: &[&str] = &["most_liquid", "bellman_ford"];

/// Default algorithm to use if none specified.
pub(crate) const DEFAULT_ALGORITHM: &str = "most_liquid";

/// Parameters for spawning workers.
pub(crate) struct SpawnWorkersParams {
    /// Algorithm name (e.g., "most_liquid") — used for thread naming and logging.
    pub algorithm: String,
    /// Number of worker threads to spawn.
    pub num_workers: usize,
    /// Configuration for the algorithm used by each worker.
    pub algorithm_config: AlgorithmConfig,
    /// Receiver for solve tasks.
    pub task_rx: async_channel::Receiver<SolveTask>,
    /// Shared market data reference.
    pub market_data: SharedMarketDataRef,
    /// Shared derived data reference (pool depths, token prices).
    pub derived_data: SharedDerivedDataRef,
    /// Broadcast receiver for market events.
    pub event_rx: broadcast::Receiver<MarketEvent>,
    /// Broadcast receiver for derived data events (resubscribed per worker).
    pub derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    /// Sender for shutdown signals.
    pub shutdown_tx: broadcast::Sender<()>,
}

/// Error returned when algorithm registration fails.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown algorithm '{name}'. Available: {}", AVAILABLE_ALGORITHMS.join(", "))]
pub struct UnknownAlgorithmError {
    /// The algorithm name that was not found.
    pub(crate) name: String,
}

impl UnknownAlgorithmError {
    /// Returns the algorithm name that was not found.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Determines how a worker pool spawns its workers.
///
/// - `Registry`: looks up a built-in algorithm by name.
/// - `Custom`: uses a [`SpawnerHandle`] loaded from a participant `.so`.
pub(crate) enum AlgorithmSpawner {
    /// Spawn workers using a built-in algorithm looked up by name.
    Registry { algorithm: String },
    /// Spawn workers using a type-erased handle from a dynamically loaded library.
    Custom(SpawnerHandle),
}

impl std::fmt::Debug for AlgorithmSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry { algorithm } => f
                .debug_struct("Registry")
                .field("algorithm", algorithm)
                .finish(),
            Self::Custom(handle) => f
                .debug_struct("Custom")
                .field("algorithm", &handle.name())
                .finish(),
        }
    }
}

impl AlgorithmSpawner {
    /// Spawns workers, dispatching to the built-in registry or the custom handle.
    pub(crate) fn spawn(
        self,
        params: SpawnWorkersParams,
    ) -> Result<Vec<JoinHandle<()>>, UnknownAlgorithmError> {
        match self {
            Self::Registry { algorithm } => match algorithm.as_str() {
                "most_liquid" => Ok(spawn_most_liquid_workers(params)),
                "bellman_ford" => Ok(spawn_bellman_ford_workers(params)),
                _ => Err(UnknownAlgorithmError { name: algorithm }),
            },
            Self::Custom(handle) => Ok(handle.spawn(params)),
        }
    }

    /// Returns the algorithm name associated with this spawner.
    pub(crate) fn algorithm_name(&self) -> &str {
        match self {
            Self::Registry { algorithm } => algorithm,
            Self::Custom(handle) => handle.name(),
        }
    }
}

/// Generic worker spawning logic.
///
/// This handles the common parts of spawning workers:
/// - Creating threads with proper names
/// - Setting up tokio runtimes
/// - Initializing graphs and running worker loops
///
/// The `factory` closure is called once per worker to create the algorithm instance.
/// It is borrowed rather than consumed, so callers (including type-erased spawner closures)
/// can call this function without giving up ownership of the factory.
pub(crate) fn spawn_workers_generic<A, F>(
    params: SpawnWorkersParams,
    factory: &F,
) -> Vec<JoinHandle<()>>
where
    A: crate::algorithm::Algorithm + 'static,
    A::GraphManager:
        crate::feed::events::MarketEventHandler + crate::graph::EdgeWeightUpdaterWithDerived,
    F: Fn(AlgorithmConfig) -> A + Clone + Send + Sync + 'static,
{
    let mut workers = Vec::with_capacity(params.num_workers);

    for worker_id in 0..params.num_workers {
        let task_rx = params.task_rx.clone();
        let market_data = Arc::clone(&params.market_data);
        let derived_data = Arc::clone(&params.derived_data);
        let event_rx = params.event_rx.resubscribe();
        let derived_event_rx = params.derived_event_rx.resubscribe();
        let algorithm_config = params.algorithm_config.clone();
        let shutdown_rx = params.shutdown_tx.subscribe();
        let algorithm_name = params.algorithm.clone();
        let factory = factory.clone();

        let handle = thread::Builder::new()
            .name(format!("{}-worker-{}", algorithm_name, worker_id))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime");

                rt.block_on(async move {
                    let algorithm = factory(algorithm_config);

                    let mut worker =
                        SolverWorker::new(market_data, derived_data, algorithm, worker_id);

                    worker.initialize_graph().await;
                    worker
                        .run(event_rx, derived_event_rx, task_rx, shutdown_rx)
                        .await;
                });
            })
            .expect("failed to spawn worker thread");

        workers.push(handle);
    }

    info!(
        algorithm = %params.algorithm,
        num_workers = params.num_workers,
        "spawned workers"
    );

    workers
}

/// Spawns workers for the MostLiquid algorithm.
fn spawn_most_liquid_workers(params: SpawnWorkersParams) -> Vec<JoinHandle<()>> {
    let factory = |config: AlgorithmConfig| {
        MostLiquidAlgorithm::with_config(config)
            .expect("invalid worker configuration for MostLiquidAlgorithm")
    };
    spawn_workers_generic(params, &factory)
}

/// Spawns workers for the BellmanFord algorithm.
fn spawn_bellman_ford_workers(params: SpawnWorkersParams) -> Vec<JoinHandle<()>> {
    let factory = |config: AlgorithmConfig| {
        BellmanFordAlgorithm::with_config(config)
            .expect("invalid worker configuration for BellmanFordAlgorithm")
    };
    spawn_workers_generic(params, &factory)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::RwLock;

    use super::*;
    use crate::{derived::DerivedData, feed::market_data::SharedMarketData};

    fn make_params(algorithm: &str, num_workers: usize) -> SpawnWorkersParams {
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let derived_data = Arc::new(RwLock::new(DerivedData::new()));
        let (_event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);
        SpawnWorkersParams {
            algorithm: algorithm.to_string(),
            num_workers,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx,
        }
    }

    #[test]
    fn test_registry_unknown_algorithm_returns_error() {
        let params = make_params("unknown_algorithm", 1);
        let result =
            AlgorithmSpawner::Registry { algorithm: "unknown_algorithm".to_string() }.spawn(params);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.name, "unknown_algorithm");
        assert!(err
            .to_string()
            .contains("unknown_algorithm"));
        assert!(err.to_string().contains("most_liquid"));
    }

    #[test]
    fn test_registry_spawns_correct_number_of_workers() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let derived_data = Arc::new(RwLock::new(DerivedData::new()));
        let (event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);

        let params = SpawnWorkersParams {
            algorithm: "most_liquid".to_string(),
            num_workers: 3,
            algorithm_config: AlgorithmConfig::new(1, 2, Duration::from_millis(50), None).unwrap(),
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx: shutdown_tx.clone(),
        };

        let workers =
            AlgorithmSpawner::Registry { algorithm: "most_liquid".to_string() }.spawn(params);
        assert!(workers.is_ok());
        let workers = workers.unwrap();
        assert_eq!(workers.len(), 3);

        // Shutdown workers gracefully
        let _ = shutdown_tx.send(());
        drop(event_tx);

        for handle in workers {
            // Give workers time to shutdown, then check they finished
            let _ = handle.join();
        }
    }

    #[test]
    fn test_custom_spawner_bypasses_registry_for_unknown_names() {
        // "my_custom_algo" is not registered — the registry would reject it.
        // The Custom spawner bypasses the registry and uses the factory directly.
        let (shutdown_tx, _) = broadcast::channel(1);
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let derived_data = Arc::new(RwLock::new(DerivedData::new()));
        let (event_tx, _) = broadcast::channel::<MarketEvent>(10);
        let (derived_event_tx, _) = broadcast::channel(10);

        let registry_err = AlgorithmSpawner::Registry { algorithm: "my_custom_algo".to_string() }
            .spawn(SpawnWorkersParams {
                algorithm: "my_custom_algo".to_string(),
                num_workers: 1,
                algorithm_config: AlgorithmConfig::default(),
                task_rx: task_rx.clone(),
                market_data: Arc::clone(&market_data),
                derived_data: Arc::clone(&derived_data),
                event_rx: event_tx.subscribe(),
                derived_event_rx: derived_event_tx.subscribe(),
                shutdown_tx: shutdown_tx.clone(),
            });
        assert!(registry_err.is_err());

        // Using MostLiquid anyway for simplicity - not to have to define a new algorithm from
        // scratch
        let handle = SpawnerHandle::new("my_custom_algo", |config: AlgorithmConfig| {
            MostLiquidAlgorithm::with_config(config).expect("invalid config in test custom spawner")
        });

        let workers = AlgorithmSpawner::Custom(handle).spawn(SpawnWorkersParams {
            algorithm: "my_custom_algo".to_string(),
            num_workers: 2,
            algorithm_config: AlgorithmConfig::new(1, 2, Duration::from_millis(50), None).unwrap(),
            task_rx,
            market_data,
            derived_data,
            event_rx: event_tx.subscribe(),
            derived_event_rx: derived_event_tx.subscribe(),
            shutdown_tx: shutdown_tx.clone(),
        });

        assert!(workers.is_ok());
        assert_eq!(workers.unwrap().len(), 2);

        let _ = shutdown_tx.send(());
    }
}
