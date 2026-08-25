//! Worker-pool algorithm registry and fallible worker spawning.
use std::{
    panic::{self, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread::{self, JoinHandle},
};

use tokio::sync::broadcast;

use crate::{
    algorithm::{
        path_frank_wolfe::PathFrankWolfeConfig, AlgorithmConfig, BellmanFordAlgorithm,
        MostLiquidAlgorithm, PathFrankWolfeAlgorithm, WaterFillAlgorithm,
    },
    derived::{events::DerivedDataEvent, SharedDerivedDataRef},
    feed::{events::MarketEvent, market_data::MarketData},
    propamm_fallback::SharedFeeTiers,
    types::internal::SolveTask,
    worker_pool::{
        pool::{safe_panic_text, spawn_thread, PoolState, WorkerFailureReason},
        worker::{SolverWorker, WorkerRunExit},
    },
    worker_pool_router::LiquidityScope,
};

/// List of available built-in algorithm names.
pub(crate) const AVAILABLE_ALGORITHMS: &[&str] =
    &["most_liquid", "bellman_ford", "path_frank_wolfe", "water_fill"];
/// Default algorithm to use if none is configured.
pub(crate) const DEFAULT_ALGORITHM: &str = "most_liquid";

/// Parameters shared by every worker spawned for one pool.
pub(crate) struct SpawnWorkersParams {
    pub algorithm: String,
    pub pool_name: String,
    pub num_workers: usize,
    pub algorithm_config: AlgorithmConfig,
    pub task_rx: async_channel::Receiver<SolveTask>,
    pub market_data: MarketData,
    pub derived_data: SharedDerivedDataRef,
    pub event_rx: broadcast::Receiver<MarketEvent>,
    pub derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub liquidity_scope: LiquidityScope,
    pub fallback_fee_tiers: SharedFeeTiers,
    pub state: Arc<PoolState>,
    pub exit_tx: Sender<()>,
}

/// Error returned when an algorithm name is not registered.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown algorithm '{name}'. Available: {}", AVAILABLE_ALGORITHMS.join(", "))]
pub struct UnknownAlgorithmError {
    pub(crate) name: String,
}

impl UnknownAlgorithmError {
    /// Returns the unknown algorithm name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Error returned while constructing a worker pool.
#[derive(Debug, thiserror::Error)]
pub enum WorkerPoolSpawnError {
    /// The pool was configured without any worker threads.
    #[error("worker pool must contain at least one worker")]
    ZeroWorkers,
    /// The configured algorithm is not registered.
    #[error(transparent)]
    UnknownAlgorithm(#[from] UnknownAlgorithmError),
    /// The operating system rejected a worker or supervisor thread.
    #[error("failed to spawn {role} thread: {source}")]
    ThreadSpawn {
        /// The failed thread's role.
        role: &'static str,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
}

type CustomSpawner =
    dyn Fn(SpawnWorkersParams) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError> + Send + Sync;

/// Determines how a pool spawns workers.
pub(crate) enum AlgorithmSpawner {
    /// Spawn a built-in algorithm.
    Registry { algorithm: String },
    /// Spawn a caller-supplied algorithm factory.
    Custom { algorithm: String, spawner: Box<CustomSpawner> },
}

impl std::fmt::Debug for AlgorithmSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry { algorithm } => f
                .debug_tuple("Registry")
                .field(algorithm)
                .finish(),
            Self::Custom { algorithm, .. } => f
                .debug_tuple("Custom")
                .field(algorithm)
                .finish(),
        }
    }
}

impl AlgorithmSpawner {
    pub(crate) fn spawn(
        self,
        params: SpawnWorkersParams,
    ) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError> {
        match self {
            Self::Registry { algorithm } => match algorithm.as_str() {
                "most_liquid" => spawn_most_liquid_workers(params),
                "bellman_ford" => spawn_bellman_ford_workers(params),
                "path_frank_wolfe" => spawn_path_frank_wolfe_workers(params),
                "water_fill" => spawn_water_fill_workers(params),
                _ => Err(UnknownAlgorithmError { name: algorithm }.into()),
            },
            Self::Custom { spawner, .. } => spawner(params),
        }
    }

    pub(crate) fn algorithm_name(&self) -> &str {
        match self {
            Self::Registry { algorithm } | Self::Custom { algorithm, .. } => algorithm,
        }
    }
}

/// Spawns workers and arranges for every worker to report its terminal outcome to the pool monitor.
pub(crate) fn spawn_workers_generic<A, F>(
    params: SpawnWorkersParams,
    factory: &F,
) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError>
where
    A: crate::algorithm::Algorithm + 'static,
    A::GraphManager:
        crate::feed::events::MarketEventHandler + crate::graph::EdgeWeightUpdaterWithDerived,
    F: Fn(AlgorithmConfig) -> A + Clone + Send + Sync + 'static,
{
    let mut workers = Vec::with_capacity(params.num_workers);
    for worker_id in 0..params.num_workers {
        let task_rx = params.task_rx.clone();
        let market_data = params.market_data.clone();
        let derived_data = Arc::clone(&params.derived_data);
        let event_rx = params.event_rx.resubscribe();
        let derived_event_rx = params.derived_event_rx.resubscribe();
        let algorithm_config = params.algorithm_config.clone();
        let shutdown_rx = params.shutdown_tx.subscribe();
        let algorithm_name = params.algorithm.clone();
        let pool_name = params.pool_name.clone();
        let factory = factory.clone();
        let liquidity_scope = params.liquidity_scope;
        let fallback_fee_tiers = params.fallback_fee_tiers.clone();
        let state = Arc::clone(&params.state);
        let state_in_worker = Arc::clone(&state);
        let exit_tx = params.exit_tx.clone();
        let started = Arc::new(AtomicBool::new(false));
        let started_in_worker = Arc::clone(&started);

        let worker = spawn_thread(
            &params.state,
            "worker",
            thread::Builder::new().name(format!("{algorithm_name}-worker-{worker_id}")),
            move || {
                let report = panic::catch_unwind(AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| WorkerFailureReason::Startup(error.to_string()))?;
                    runtime.block_on(async move {
                        let algorithm = factory(algorithm_config);
                        let mut worker = SolverWorker::new(
                            market_data,
                            derived_data,
                            algorithm,
                            worker_id,
                            pool_name,
                        )
                        .with_liquidity_scope(liquidity_scope)
                        .with_fallback_fee_tiers(fallback_fee_tiers);
                        if state_in_worker.panic_during_initialization() {
                            panic!("injected worker initialization panic");
                        }
                        worker.initialize_graph().await;
                        if state_in_worker.shutdown_requested() {
                            return Ok(WorkerRunExit::Shutdown);
                        }
                        state_in_worker.worker_started();
                        started_in_worker.store(true, Ordering::Release);
                        if state_in_worker.panic_after_startup() {
                            panic!("injected worker panic after startup");
                        }
                        Ok(worker
                            .run(event_rx, derived_event_rx, task_rx, shutdown_rx)
                            .await)
                    })
                }));
                let failure_reason = match report {
                    Ok(Ok(WorkerRunExit::Shutdown)) => None,
                    Ok(Ok(WorkerRunExit::InputClosed)) => Some(WorkerFailureReason::Returned),
                    Ok(Err(reason)) => Some(reason),
                    Err(panic) => Some(WorkerFailureReason::Panic(safe_panic_text(panic))),
                };
                let started = started.load(Ordering::Acquire);
                state.worker_stopped(started);
                if let Some(reason) = failure_reason {
                    state.report_worker_failure(worker_id, reason);
                }
                let _ = exit_tx.send(());
            },
        );

        match worker {
            Ok(worker) => workers.push(worker),
            Err(source) => {
                params.state.request_shutdown();
                for worker in workers {
                    let _ = worker.join();
                }
                return Err(WorkerPoolSpawnError::ThreadSpawn { role: "worker", source });
            }
        }
    }
    Ok(workers)
}

fn spawn_most_liquid_workers(
    params: SpawnWorkersParams,
) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError> {
    let factory = |config: AlgorithmConfig| {
        MostLiquidAlgorithm::with_config(config)
            .unwrap_or_else(|error| panic!("invalid worker configuration: {error}"))
    };
    spawn_workers_generic(params, &factory)
}

fn spawn_bellman_ford_workers(
    params: SpawnWorkersParams,
) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError> {
    spawn_workers_generic(params, &BellmanFordAlgorithm::with_config)
}

fn spawn_path_frank_wolfe_workers(
    params: SpawnWorkersParams,
) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError> {
    let factory = |config: AlgorithmConfig| {
        PathFrankWolfeAlgorithm::new(config, PathFrankWolfeConfig::default())
    };
    spawn_workers_generic(params, &factory)
}

fn spawn_water_fill_workers(
    params: SpawnWorkersParams,
) -> Result<Vec<JoinHandle<()>>, WorkerPoolSpawnError> {
    let factory = |config: AlgorithmConfig| {
        WaterFillAlgorithm::with_config(config)
            .unwrap_or_else(|error| panic!("invalid worker configuration: {error}"))
    };
    spawn_workers_generic(params, &factory)
}
