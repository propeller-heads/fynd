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

use tokio::sync::broadcast;
use tracing::info;

use crate::{
    algorithm::{
        path_frank_wolfe::PathFrankWolfeConfig, AlgorithmConfig, BellmanFordAlgorithm,
        MostLiquidAlgorithm, PathFrankWolfeAlgorithm, WaterFillAlgorithm,
    },
    derived::{events::DerivedDataEvent, SharedDerivedDataRef},
    feed::{events::MarketEvent, market_data::MarketData},
    propamm_fallback::SharedFeeTiers,
    types::internal::SolveTask,
    worker_pool::supervisor::{RespawnPolicy, WorkerContext},
    worker_pool_router::LiquidityScope,
};

/// List of available built-in algorithm names (for registry-based dispatch).
pub(crate) const AVAILABLE_ALGORITHMS: &[&str] =
    &["most_liquid", "bellman_ford", "path_frank_wolfe", "water_fill"];

/// Default algorithm to use if none specified.
pub(crate) const DEFAULT_ALGORITHM: &str = "most_liquid";

/// Parameters for spawning workers.
pub(crate) struct SpawnWorkersParams {
    /// Algorithm name (e.g., "most_liquid") — used for thread naming and logging.
    pub algorithm: String,
    /// Worker pool name from configuration (used as the `pool` metric label).
    pub pool_name: String,
    /// Number of worker threads to spawn.
    pub num_workers: usize,
    /// Configuration for the algorithm used by each worker.
    pub algorithm_config: AlgorithmConfig,
    /// Receiver for solve tasks.
    pub task_rx: async_channel::Receiver<SolveTask>,
    /// Shared market data reference.
    pub market_data: MarketData,
    /// Shared derived data reference (component depths, token prices).
    pub derived_data: SharedDerivedDataRef,
    /// Broadcast receiver for market events.
    pub event_rx: broadcast::Receiver<MarketEvent>,
    /// Broadcast receiver for derived data events (resubscribed per worker).
    pub derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    /// Sender for shutdown signals.
    pub shutdown_tx: broadcast::Sender<()>,
    /// Liquidity scope applied to every worker in this worker pool.
    pub liquidity_scope: LiquidityScope,
    /// Protocol systems every worker in this worker pool leaves out of its graph.
    pub exclude_protocols: Vec<String>,
    /// PropAMMRouter fee tiers, shared with the fetcher that refreshes them.
    pub fallback_fee_tiers: SharedFeeTiers,
    /// Retry policy for respawning panicked workers.
    pub respawn_policy: RespawnPolicy,
    /// Called when a worker gives up respawning.
    pub on_worker_gave_up: Arc<dyn Fn() + Send + Sync>,
}

/// Error returned when algorithm registration fails.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "unknown algorithm '{name}'. Available: {}",
    AVAILABLE_ALGORITHMS
        .iter()
        .copied()
        .chain(registered.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(", ")
)]
pub struct UnknownAlgorithmError {
    /// The algorithm name that was not found.
    pub(crate) name: String,
    /// Names the caller registered, so the message lists what was really on offer rather than
    /// only what ships here.
    pub(crate) registered: Vec<String>,
}

impl UnknownAlgorithmError {
    /// The algorithm name that was not found.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Names a caller registered alongside the built-ins.
    pub(crate) fn of(name: impl Into<String>, registered: Vec<String>) -> Self {
        Self { name: name.into(), registered }
    }
}

/// Determines how a worker pool spawns its workers.
///
/// - `Registry`: looks up a built-in algorithm by name.
/// - `Custom`: uses a caller-supplied factory closure, bypassing the registry.
pub(crate) enum AlgorithmSpawner {
    /// Spawn workers using a built-in algorithm looked up by name.
    Registry { algorithm: String },
    /// Spawn workers using a custom factory function (type-erased).
    Custom {
        algorithm: String,
        spawner: Box<dyn Fn(SpawnWorkersParams) -> Vec<JoinHandle<()>> + Send + Sync>,
    },
}

impl std::fmt::Debug for AlgorithmSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry { algorithm } => f
                .debug_struct("Registry")
                .field("algorithm", algorithm)
                .finish(),
            Self::Custom { algorithm, .. } => f
                .debug_struct("Custom")
                .field("algorithm", algorithm)
                .finish(),
        }
    }
}

impl AlgorithmSpawner {
    /// Spawns workers, dispatching to the registry or custom spawner as appropriate.
    pub(crate) fn spawn(
        self,
        params: SpawnWorkersParams,
    ) -> Result<Vec<JoinHandle<()>>, UnknownAlgorithmError> {
        match self {
            Self::Registry { algorithm } => match algorithm.as_str() {
                "most_liquid" => Ok(spawn_most_liquid_workers(params)),
                "bellman_ford" => Ok(spawn_bellman_ford_workers(params)),
                "path_frank_wolfe" => Ok(spawn_path_frank_wolfe_workers(params)),
                "water_fill" => Ok(spawn_water_fill_workers(params)),
                _ => Err(UnknownAlgorithmError::of(algorithm, Vec::new())),
            },
            Self::Custom { spawner, .. } => Ok(spawner(params)),
        }
    }

    /// Returns the algorithm name associated with this spawner.
    /// Whether the algorithm came from a caller rather than the built-in list.
    #[cfg(test)]
    pub(crate) fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }

    pub(crate) fn algorithm_name(&self) -> &str {
        match self {
            Self::Registry { algorithm } | Self::Custom { algorithm, .. } => algorithm,
        }
    }
}

/// Generic worker spawning logic.
///
/// Each worker thread runs sessions in a loop (see [`WorkerContext::run_sessions`]):
/// a panic ends the current session and the worker is respawned after a backoff,
/// giving up after repeated rapid failures. The `factory` closure is called at
/// every session (re)start, so it must tolerate repeated calls. It is borrowed
/// rather than consumed, so callers (including type-erased spawner closures)
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
        let ctx = WorkerContext {
            worker_id,
            algorithm_name: params.algorithm.clone(),
            pool_name: params.pool_name.clone(),
            factory: factory.clone(),
            algorithm_config: params.algorithm_config.clone(),
            market_data: params.market_data.clone(),
            derived_data: Arc::clone(&params.derived_data),
            task_rx: params.task_rx.clone(),
            event_rx: params.event_rx.resubscribe(),
            derived_event_rx: params.derived_event_rx.resubscribe(),
            // Subscribed before the thread starts so shutdown signals sent at any
            // point, including while the worker is recovering from a panic, are
            // never missed.
            shutdown_rx: params.shutdown_tx.subscribe(),
            liquidity_scope: params.liquidity_scope,
            exclude_protocols: params.exclude_protocols.clone(),
            fallback_fee_tiers: params.fallback_fee_tiers.clone(),
            respawn_policy: params.respawn_policy,
            on_worker_gave_up: Arc::clone(&params.on_worker_gave_up),
        };

        let handle = thread::Builder::new()
            .name(format!("{}-worker-{}", params.algorithm, worker_id))
            .spawn(move || ctx.run_sessions())
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
    let factory = |config: AlgorithmConfig| BellmanFordAlgorithm::with_config(config);
    spawn_workers_generic(params, &factory)
}

/// Spawns workers for the PathFrankWolfe split-routing algorithm.
fn spawn_path_frank_wolfe_workers(params: SpawnWorkersParams) -> Vec<JoinHandle<()>> {
    let factory = |config: AlgorithmConfig| {
        PathFrankWolfeAlgorithm::new(config, PathFrankWolfeConfig::default())
    };
    spawn_workers_generic(params, &factory)
}

/// Spawns workers for the water-fill portfolio split-routing algorithm.
fn spawn_water_fill_workers(params: SpawnWorkersParams) -> Vec<JoinHandle<()>> {
    let factory = |config: AlgorithmConfig| {
        WaterFillAlgorithm::with_config(config)
            .expect("invalid worker configuration for WaterFillAlgorithm")
    };
    spawn_workers_generic(params, &factory)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use num_bigint::BigUint;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::*;
    use crate::{
        algorithm::{
            most_liquid::DepthAndPrice,
            test_utils::{order, setup_market_weighted, token},
            Algorithm, AlgorithmError,
        },
        derived::{computation::ComputationRequirements, DerivedData},
        feed::market_data::{MarketData, StateLabel},
        graph::petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
        types::{quote::OrderSide, Order, RouteResult, SolveError},
    };

    fn make_params(algorithm: &str, num_workers: usize) -> SpawnWorkersParams {
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);
        SpawnWorkersParams {
            algorithm: algorithm.to_string(),
            pool_name: "test_pool".to_string(),
            num_workers,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx,
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy::default(),
            on_worker_gave_up: Arc::new(|| {}),
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
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);

        let params = SpawnWorkersParams {
            algorithm: "most_liquid".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 3,
            algorithm_config: AlgorithmConfig::new(1, 2, Duration::from_millis(50), None).unwrap(),
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy::default(),
            on_worker_gave_up: Arc::new(|| {}),
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
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (event_tx, _) = broadcast::channel::<MarketEvent>(10);
        let (derived_event_tx, _) = broadcast::channel(10);

        let registry_err = AlgorithmSpawner::Registry { algorithm: "my_custom_algo".to_string() }
            .spawn(SpawnWorkersParams {
                algorithm: "my_custom_algo".to_string(),
                pool_name: "test_pool".to_string(),
                num_workers: 1,
                algorithm_config: AlgorithmConfig::default(),
                task_rx: task_rx.clone(),
                market_data: market_data.clone(),
                derived_data: Arc::clone(&derived_data),
                event_rx: event_tx.subscribe(),
                derived_event_rx: derived_event_tx.subscribe(),
                shutdown_tx: shutdown_tx.clone(),
                liquidity_scope: LiquidityScope::default(),
                exclude_protocols: Vec::new(),
                fallback_fee_tiers: SharedFeeTiers::default(),
                respawn_policy: RespawnPolicy::default(),
                on_worker_gave_up: Arc::new(|| {}),
            });
        assert!(registry_err.is_err());

        // Using MostLiquid anyway for simplicity - not to have to define a new algorithm from
        // scratch
        let spawner: Box<dyn Fn(SpawnWorkersParams) -> Vec<JoinHandle<()>> + Send + Sync> =
            Box::new(|p| {
                let factory = |config: AlgorithmConfig| {
                    MostLiquidAlgorithm::with_config(config)
                        .expect("invalid config in test custom spawner")
                };
                spawn_workers_generic(p, &factory)
            });

        let workers = AlgorithmSpawner::Custom { algorithm: "my_custom_algo".to_string(), spawner }
            .spawn(SpawnWorkersParams {
                algorithm: "my_custom_algo".to_string(),
                pool_name: "test_pool".to_string(),
                num_workers: 2,
                algorithm_config: AlgorithmConfig::new(1, 2, Duration::from_millis(50), None)
                    .unwrap(),
                task_rx,
                market_data,
                derived_data,
                event_rx: event_tx.subscribe(),
                derived_event_rx: derived_event_tx.subscribe(),
                shutdown_tx: shutdown_tx.clone(),
                liquidity_scope: LiquidityScope::default(),
                exclude_protocols: Vec::new(),
                fallback_fee_tiers: SharedFeeTiers::default(),
                respawn_policy: RespawnPolicy::default(),
                on_worker_gave_up: Arc::new(|| {}),
            });

        assert!(workers.is_ok());
        assert_eq!(workers.unwrap().len(), 2);

        let _ = shutdown_tx.send(());
    }

    /// Amount marking the order whose solve panics in [`PanicOnPoisonAlgorithm`].
    const POISON_AMOUNT: u128 = 666;

    /// Algorithm that panics while solving the poison order and returns an error
    /// otherwise. Used to verify that a panicking task does not permanently lose the
    /// worker: the worker respawns.
    #[derive(Clone)]
    struct PanicOnPoisonAlgorithm;

    impl Algorithm for PanicOnPoisonAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "panic_on_poison"
        }

        async fn find_best_route(
            &self,
            _graph: &Self::GraphType,
            _market: MarketData,
            _label: Option<StateLabel>,
            _derived: Option<crate::derived::SharedDerivedDataRef>,
            order: &Order,
        ) -> Result<RouteResult, AlgorithmError> {
            if order.amount() == &BigUint::from(POISON_AMOUNT) {
                panic!("poison order");
            }
            Err(AlgorithmError::Other("no route in mock".to_string()))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            ComputationRequirements::none()
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    #[tokio::test]
    async fn worker_respawns_after_panic_and_processes_next_task() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived_data = DerivedData::new_shared();
        let (task_tx, task_rx) = async_channel::bounded(10);
        let (event_tx, _) = broadcast::channel::<MarketEvent>(10);
        let (derived_event_tx, _) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = SpawnWorkersParams {
            algorithm: "panic_on_poison".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data: market,
            derived_data,
            event_rx: event_tx.subscribe(),
            derived_event_rx: derived_event_tx.subscribe(),
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy::default(),
            on_worker_gave_up: Arc::new(|| {}),
        };
        let factory = |_config: AlgorithmConfig| PanicOnPoisonAlgorithm;
        let workers = spawn_workers_generic(params, &factory);

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // The poison task panics mid-solve; its response channel is dropped.
        let (poison_tx, poison_rx) = oneshot::channel();
        let poison_order = order(&token_a, &token_b, POISON_AMOUNT, OrderSide::Sell);
        task_tx
            .send(SolveTask::new(Uuid::new_v4(), poison_order, poison_tx))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), poison_rx)
            .await
            .expect("poison task should be picked up")
            .expect_err("poison task must panic, not respond");

        // The worker must come back and answer the next task.
        let (normal_tx, normal_rx) = oneshot::channel();
        let normal_order = order(&token_a, &token_b, 100, OrderSide::Sell);
        task_tx
            .send(SolveTask::new(Uuid::new_v4(), normal_order, normal_tx))
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), normal_rx)
            .await
            .expect("worker should respawn after the panic and process the next task")
            .expect("worker should respond to the task");
        match response {
            Err(SolveError::AlgorithmError(msg)) => {
                assert!(msg.contains("no route in mock"), "unexpected message: {msg}");
            }
            other => panic!("expected AlgorithmError from mock, got {other:?}"),
        }

        let _ = shutdown_tx.send(());
        drop(task_tx);
        for handle in workers {
            handle
                .join()
                .expect("worker thread should shut down cleanly");
        }
    }

    #[tokio::test]
    async fn workers_exit_when_pool_drops_shutdown_sender() {
        let (market, _) = setup_market_weighted(vec![]);
        let (_task_tx, task_rx) = async_channel::bounded::<SolveTask>(10);
        let (event_tx, _) = broadcast::channel::<MarketEvent>(10);
        let (derived_event_tx, _) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = SpawnWorkersParams {
            algorithm: "panic_on_poison".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data: market,
            derived_data: DerivedData::new_shared(),
            event_rx: event_tx.subscribe(),
            derived_event_rx: derived_event_tx.subscribe(),
            shutdown_tx,
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy::default(),
            on_worker_gave_up: Arc::new(|| {}),
        };
        let factory = |_config: AlgorithmConfig| PanicOnPoisonAlgorithm;
        let workers = spawn_workers_generic(params, &factory);
        // `params` (holding the only shutdown sender) is consumed and dropped above.

        for handle in workers {
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || handle.join()),
            )
            .await
            .expect("worker should exit when the shutdown sender drops")
            .unwrap()
            .expect("worker thread should exit cleanly");
        }
    }

    #[tokio::test]
    async fn worker_gives_up_after_repeated_rapid_panics() {
        let (market, _) = setup_market_weighted(vec![]);
        let (_task_tx, task_rx) = async_channel::bounded::<SolveTask>(10);
        let (event_tx, _) = broadcast::channel::<MarketEvent>(10);
        let (derived_event_tx, _) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);
        let gave_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gave_up_flag = Arc::clone(&gave_up);

        let params = SpawnWorkersParams {
            algorithm: "panic_on_poison".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data: market,
            derived_data: DerivedData::new_shared(),
            event_rx: event_tx.subscribe(),
            derived_event_rx: derived_event_tx.subscribe(),
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy {
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(2),
                max_attempts: 3,
                stable_session: Duration::from_secs(60),
            },
            on_worker_gave_up: Arc::new(move || {
                gave_up_flag.store(true, std::sync::atomic::Ordering::SeqCst)
            }),
        };
        let factory = |_config: AlgorithmConfig| -> PanicOnPoisonAlgorithm {
            panic!("deterministic init panic")
        };
        let workers = spawn_workers_generic(params, &factory);

        for handle in workers {
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || handle.join()),
            )
            .await
            .expect("worker should give up instead of retrying forever")
            .unwrap()
            .expect("worker thread should exit cleanly after giving up");
        }
        assert!(gave_up.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_sent_mid_respawn_is_not_lost() {
        let (market, _) = setup_market_weighted(vec![]);
        let derived_data = DerivedData::new_shared();
        let (task_tx, task_rx) = async_channel::bounded(10);
        let (event_tx, _) = broadcast::channel::<MarketEvent>(10);
        let (derived_event_tx, _) = broadcast::channel(10);
        let (shutdown_tx, _) = broadcast::channel(1);

        let params = SpawnWorkersParams {
            algorithm: "panic_on_poison".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data: market,
            derived_data,
            event_rx: event_tx.subscribe(),
            derived_event_rx: derived_event_tx.subscribe(),
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy {
                initial_backoff: Duration::from_millis(500),
                ..RespawnPolicy::default()
            },
            on_worker_gave_up: Arc::new(|| {}),
        };
        let factory = |_config: AlgorithmConfig| PanicOnPoisonAlgorithm;
        let workers = spawn_workers_generic(params, &factory);

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");

        // The poison task panics mid-solve; its response channel is dropped. The worker
        // now sleeps through its 500ms backoff with no session listening for shutdown.
        let (poison_tx, poison_rx) = oneshot::channel();
        let poison_order = order(&token_a, &token_b, POISON_AMOUNT as u128, OrderSide::Sell);
        task_tx
            .send(SolveTask::new(Uuid::new_v4(), poison_order, poison_tx))
            .await
            .unwrap();
        assert!(poison_rx.await.is_err());

        // Sent while the worker is mid-respawn (no session receiver listening yet). The
        // buffered `shutdown_rx.try_recv()` pre-check must still catch it.
        shutdown_tx.send(()).unwrap();

        for handle in workers {
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || handle.join()),
            )
            .await
            .expect("buffered shutdown sent mid-respawn should not be lost")
            .unwrap()
            .expect("worker thread should shut down cleanly");
        }

        // Keep the sender alive until after the join so the worker exits via the
        // buffered shutdown signal, not because the task channel closed.
        drop(task_tx);
    }

    #[test]
    fn test_registry_spawns_path_frank_wolfe() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);

        let params = SpawnWorkersParams {
            algorithm: "path_frank_wolfe".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy::default(),
            on_worker_gave_up: Arc::new(|| {}),
        };

        let workers =
            AlgorithmSpawner::Registry { algorithm: "path_frank_wolfe".to_string() }.spawn(params);
        assert!(workers.is_ok());
        assert_eq!(workers.unwrap().len(), 1);

        let _ = shutdown_tx.send(());
        drop(event_tx);
    }

    #[test]
    fn test_registry_spawns_water_fill() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (_task_tx, task_rx) = async_channel::bounded(10);
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (event_tx, event_rx) = broadcast::channel(10);
        let (_derived_event_tx, derived_event_rx) = broadcast::channel(10);

        let params = SpawnWorkersParams {
            algorithm: "water_fill".to_string(),
            pool_name: "test_pool".to_string(),
            num_workers: 1,
            algorithm_config: AlgorithmConfig::default(),
            task_rx,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx: shutdown_tx.clone(),
            liquidity_scope: LiquidityScope::default(),
            exclude_protocols: Vec::new(),
            fallback_fee_tiers: SharedFeeTiers::default(),
            respawn_policy: RespawnPolicy::default(),
            on_worker_gave_up: Arc::new(|| {}),
        };

        let workers =
            AlgorithmSpawner::Registry { algorithm: "water_fill".to_string() }.spawn(params);
        assert!(workers.is_ok());
        assert_eq!(workers.unwrap().len(), 1);

        let _ = shutdown_tx.send(());
        drop(event_tx);
    }
}
