//! Worker pool for processing solve tasks.
//!
//! The worker pool manages dedicated OS threads for CPU-bound route finding.
//! Each worker owns a Solver instance and processes tasks from the queue.

use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{error, info, warn};

use crate::{
    algorithm::MostLiquidAlgorithm,
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
    types::SolveTask,
    worker::{SolverWorker, WorkerConfig},
};

/// Configuration for the worker pool.
#[derive(Debug, Clone)]
pub struct WorkerPoolConfig {
    /// Number of worker threads.
    pub num_workers: usize,
    /// Configuration for each solver.
    pub worker_config: WorkerConfig,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self { num_workers: num_cpus::get(), worker_config: WorkerConfig::default() }
    }
}

/// A pool of worker threads for processing solve tasks.
pub struct WorkerPool {
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
    /// * `event_tx` - Broadcast sender for market events (workers subscribe to this)
    pub fn spawn(
        config: WorkerPoolConfig,
        task_rx: mpsc::Receiver<SolveTask>,
        market_data: SharedMarketDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);

        // Wrap task_rx in Arc<Mutex> so workers can share it
        let task_rx = Arc::new(Mutex::new(task_rx));

        let mut workers = Vec::with_capacity(config.num_workers);

        for worker_id in 0..config.num_workers {
            let task_rx = Arc::clone(&task_rx);
            let market_data = Arc::clone(&market_data);
            let event_rx = event_rx.resubscribe();
            let worker_config = config.worker_config.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();

            let handle = thread::Builder::new()
                .name(format!("solver-worker-{}", worker_id))
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

                        // Create solver (graph type and manager are automatically inferred from
                        // algorithm)
                        let mut worker =
                            SolverWorker::new(market_data, event_rx, algorithm, worker_config);

                        // Initialize solver graph
                        worker.initialize_graph().await;

                        info!(worker_id, "worker started");

                        loop {
                            tokio::select! {
                                // Check for shutdown
                                _ = shutdown_rx.recv() => {
                                    info!(worker_id, "worker shutting down");
                                    break;
                                }

                                // Get next task
                                task = async {
                                    let mut rx = task_rx.lock().await;
                                    rx.recv().await
                                } => {
                                    match task {
                                        Some(task) => {
                                            let task_id = task.id;
                                            let _wait_time = task.wait_time();

                                            // Process the task
                                            let result = worker.solve(&task.request).await;

                                            if let Err(ref e) = result {
                                                warn!(
                                                    worker_id,
                                                    task_id = %task_id,
                                                    error = %e,
                                                    "solve failed"
                                                );
                                            }

                                            // Send response
                                            task.respond(result);
                                        }
                                        None => {
                                            // Channel closed, exit
                                            info!(worker_id, "task channel closed, exiting");
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                })
                .expect("failed to spawn worker thread");

            workers.push(handle);
        }

        Self { workers, shutdown_tx }
    }

    /// Returns the number of workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Shuts down all workers and waits for them to finish.
    pub fn shutdown(self) {
        info!("shutting down worker pool");

        // Send shutdown signal
        let _ = self.shutdown_tx.send(());

        // Wait for all workers to finish
        for (i, handle) in self.workers.into_iter().enumerate() {
            if let Err(e) = handle.join() {
                error!(worker_id = i, "worker thread panicked: {:?}", e);
            }
        }

        info!("worker pool shut down");
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

    pub fn num_workers(mut self, n: usize) -> Self {
        self.config.num_workers = n;
        self
    }

    pub fn worker_config(mut self, config: WorkerConfig) -> Self {
        self.config.worker_config = config;
        self
    }

    pub fn build(
        self,
        task_rx: mpsc::Receiver<SolveTask>,
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
