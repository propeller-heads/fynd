//! Worker pool registry for spawning workers with different algorithms.
//!
//! This module provides a registry pattern for algorithms, allowing worker pools
//! to be created by algorithm name (string).
//!
//! # Adding a New Algorithm
//!
//! 1. Implement the `Algorithm` trait for your algorithm
//! 2. Add a match arm in `spawn_workers` that creates your algorithm
//! 3. Add the algorithm name to `AVAILABLE_ALGORITHMS`

use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use tokio::sync::broadcast;
use tracing::info;

use crate::{
    algorithm::{AlgorithmConfig, MostLiquidAlgorithm},
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
    types::internal::SolveTask,
    worker_pool::worker::SolverWorker,
};

/// List of available algorithm names.
pub(crate) const AVAILABLE_ALGORITHMS: &[&str] = &["most_liquid"];

/// Default algorithm to use if none specified.
pub(crate) const DEFAULT_ALGORITHM: &str = "most_liquid";

/// Returns a list of all registered algorithm names.
#[allow(dead_code)]
pub(crate) fn list_algorithms() -> &'static [&'static str] {
    AVAILABLE_ALGORITHMS
}

/// Parameters for spawning workers.
pub(crate) struct SpawnWorkersParams {
    /// Algorithm name (e.g., "most_liquid").
    pub algorithm: String,
    /// Number of worker threads to spawn.
    pub num_workers: usize,
    /// Configuration for the algorithm used by each worker.
    pub algorithm_config: AlgorithmConfig,
    /// Receiver for solve tasks.
    pub task_rx: async_channel::Receiver<SolveTask>,
    /// Shared market data reference.
    pub market_data: SharedMarketDataRef,
    /// Broadcast receiver for market events.
    pub event_rx: broadcast::Receiver<MarketEvent>,
    /// Sender for shutdown signals.
    pub shutdown_tx: broadcast::Sender<()>,
}

/// Error returned when algorithm registration fails.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown algorithm '{name}'. Available: {}", AVAILABLE_ALGORITHMS.join(", "))]
pub(crate) struct UnknownAlgorithmError {
    /// The algorithm name that was not found.
    pub name: String,
}

/// Spawns worker threads for the specified algorithm.
///
/// This is the core registry dispatch function. It matches on the algorithm name
/// and creates the appropriate algorithm instance and workers.
///
/// # Returns
///
/// Vector of join handles for the spawned worker threads, or an error if the
/// algorithm is not registered.
pub(crate) fn spawn_workers(
    params: SpawnWorkersParams,
) -> Result<Vec<JoinHandle<()>>, UnknownAlgorithmError> {
    match params.algorithm.as_str() {
        "most_liquid" => Ok(spawn_most_liquid_workers(params)),
        _ => Err(UnknownAlgorithmError { name: params.algorithm }),
    }
}

/// Generic worker spawning logic.
///
/// This handles the common parts of spawning workers:
/// - Creating threads with proper names
/// - Setting up tokio runtimes
/// - Initializing graphs and running worker loops
///
/// The `create_algorithm` closure is called once per worker to create the
/// algorithm-specific instance.
fn spawn_workers_generic<A, F>(
    params: SpawnWorkersParams,
    create_algorithm: F,
) -> Vec<JoinHandle<()>>
where
    A: crate::algorithm::Algorithm + 'static,
    A::GraphManager: crate::feed::events::MarketEventHandler,
    F: Fn(&AlgorithmConfig) -> A + Send + Sync + Clone + 'static,
{
    let mut workers = Vec::with_capacity(params.num_workers);

    for worker_id in 0..params.num_workers {
        let task_rx = params.task_rx.clone();
        let market_data = Arc::clone(&params.market_data);
        let event_rx = params.event_rx.resubscribe();
        let algorithm_config = params.algorithm_config.clone();
        let shutdown_rx = params.shutdown_tx.subscribe();
        let algorithm_name = params.algorithm.clone();
        let create_algorithm = create_algorithm.clone();

        let handle = thread::Builder::new()
            .name(format!("{}-worker-{}", algorithm_name, worker_id))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime");

                rt.block_on(async move {
                    let algorithm = create_algorithm(&algorithm_config);

                    let mut worker = SolverWorker::new(market_data, algorithm, worker_id);

                    worker.initialize_graph().await;
                    worker
                        .run(event_rx, task_rx, shutdown_rx)
                        .await;
                });
            })
            .expect("failed to spawn worker thread");

        workers.push(handle);
    }

    info!(
        algorithm = %params.algorithm,
        num_workers = params.num_workers,
        "spawned workers via registry"
    );

    workers
}

/// Spawns workers for the MostLiquid algorithm.
fn spawn_most_liquid_workers(params: SpawnWorkersParams) -> Vec<JoinHandle<()>> {
    spawn_workers_generic(params, |config| {
        MostLiquidAlgorithm::with_config(config.clone())
            .expect("invalid worker configuration for MostLiquidAlgorithm")
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::RwLock;

    use super::*;
    use crate::feed::market_data::SharedMarketData;

    #[test]
    fn test_list_algorithms() {
        let algos = list_algorithms();
        assert!(algos.contains(&"most_liquid"));
    }

    #[test]
    fn test_spawn_workers_unknown_algorithm_returns_error() {
        let (task_tx, task_rx) = async_channel::bounded(10);
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let (event_tx, event_rx) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = SpawnWorkersParams {
            algorithm: "unknown_algorithm".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data,
            event_rx,
            shutdown_tx,
        };

        let result = spawn_workers(params);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.name, "unknown_algorithm");
        assert!(err
            .to_string()
            .contains("unknown_algorithm"));
        assert!(err.to_string().contains("most_liquid")); // Should list available algorithms

        // Cleanup
        drop(task_tx);
        drop(event_tx);
    }

    #[test]
    fn test_spawn_workers_creates_correct_number_of_workers() {
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = Arc::new(RwLock::new(SharedMarketData::new()));
        let (event_tx, event_rx) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = SpawnWorkersParams {
            algorithm: "most_liquid".to_string(),
            num_workers: 3,
            algorithm_config: AlgorithmConfig::new(1, 2, Duration::from_millis(50)).unwrap(),
            task_rx,
            market_data,
            event_rx,
            shutdown_tx: shutdown_tx.clone(),
        };

        let result = spawn_workers(params);
        assert!(result.is_ok());

        let workers = result.unwrap();
        assert_eq!(workers.len(), 3);

        // Shutdown workers gracefully
        let _ = shutdown_tx.send(());
        drop(event_tx);

        for handle in workers {
            // Give workers time to shutdown, then check they finished
            let _ = handle.join();
        }
    }
}
