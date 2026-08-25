//! Worker pool for processing solve tasks.
//!
//! A pool owns dedicated worker threads for one routing algorithm. Any unexpected worker exit is
//! process-fatal: the pool stops its siblings, becomes unhealthy, and reports one typed failure to
//! its owner. Recovery is deliberately left to the external process supervisor.
use std::{
    any::Any,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

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
    propamm_fallback::SharedFeeTiers,
    types::internal::SolveTask,
    worker_pool::{
        registry::{
            spawn_workers_generic, AlgorithmSpawner, SpawnWorkersParams, WorkerPoolSpawnError,
            DEFAULT_ALGORITHM,
        },
        task_queue::{TaskQueue, TaskQueueConfig, TaskQueueHandle},
    },
    worker_pool_router::LiquidityScope,
};

/// Configuration for the worker pool.
#[derive(Debug)]
pub struct WorkerPoolConfig {
    name: String,
    spawner: AlgorithmSpawner,
    num_workers: usize,
    algorithm_config: AlgorithmConfig,
    task_queue_capacity: usize,
    liquidity_scope: LiquidityScope,
    fallback_fee_tiers: SharedFeeTiers,
    #[cfg(test)]
    thread_spawn_failure: Option<TestThreadSpawnFailure>,
    #[cfg(test)]
    panic_point: TestPanicPoint,
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
            fallback_fee_tiers: SharedFeeTiers::default(),
            #[cfg(test)]
            thread_spawn_failure: None,
            #[cfg(test)]
            panic_point: TestPanicPoint::None,
        }
    }
}

/// Test-only deterministic thread-spawn fault injection.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
struct TestThreadSpawnFailure {
    role: &'static str,
    successful_spawns_before_failure: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestPanicPoint {
    None,
    DuringInitialization,
    AfterStartup,
}

/// The reason a worker stopped unexpectedly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerFailureReason {
    /// The worker panicked. The text is safe to log but must never be a metric label.
    Panic(String),
    /// The worker returned from its run loop without an explicit shutdown request.
    Returned,
    /// The worker could not complete startup before entering its run loop.
    Startup(String),
}

impl WorkerFailureReason {
    fn metric_label(&self) -> &'static str {
        match self {
            Self::Panic(_) => "panic",
            Self::Returned => "returned",
            Self::Startup(_) => "startup",
        }
    }
}

/// A process-fatal worker-pool failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFailure {
    pool: String,
    algorithm: String,
    worker_id: usize,
    reason: WorkerFailureReason,
}

impl WorkerFailure {
    /// Returns the configured pool name.
    pub fn pool(&self) -> &str {
        &self.pool
    }

    /// Returns the configured algorithm name.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the worker identifier within the pool.
    pub fn worker_id(&self) -> usize {
        self.worker_id
    }

    /// Returns why the worker stopped.
    pub fn reason(&self) -> &WorkerFailureReason {
        &self.reason
    }
}

pub(crate) struct PoolState {
    pool: String,
    algorithm: String,
    configured_workers: usize,
    live_workers: AtomicUsize,
    shutdown_requested: AtomicBool,
    fatal_failure: Mutex<Option<WorkerFailure>>,
    shutdown_tx: broadcast::Sender<()>,
    failure_tx: broadcast::Sender<WorkerFailure>,
    #[cfg(test)]
    thread_spawn_failure: Mutex<Option<TestThreadSpawnFailure>>,
    #[cfg(test)]
    panic_point: TestPanicPoint,
}

impl PoolState {
    fn new(
        pool: String,
        algorithm: String,
        configured_workers: usize,
        shutdown_tx: broadcast::Sender<()>,
        failure_tx: broadcast::Sender<WorkerFailure>,
        #[cfg(test)] thread_spawn_failure: Option<TestThreadSpawnFailure>,
        #[cfg(test)] panic_point: TestPanicPoint,
    ) -> Self {
        Self {
            pool,
            algorithm,
            configured_workers,
            live_workers: AtomicUsize::new(0),
            shutdown_requested: AtomicBool::new(false),
            fatal_failure: Mutex::new(None),
            shutdown_tx,
            failure_tx,
            #[cfg(test)]
            thread_spawn_failure: Mutex::new(thread_spawn_failure),
            #[cfg(test)]
            panic_point,
        }
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
            .load(Ordering::Acquire)
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown_requested
            .store(true, Ordering::Release);
        let _ = self.shutdown_tx.send(());
    }

    pub(crate) fn worker_started(&self) {
        self.live_workers
            .fetch_add(1, Ordering::AcqRel);
        self.record_health_metrics();
    }

    #[cfg(test)]
    pub(crate) fn panic_during_initialization(&self) -> bool {
        self.panic_point == TestPanicPoint::DuringInitialization
    }

    #[cfg(not(test))]
    pub(crate) fn panic_during_initialization(&self) -> bool {
        false
    }

    #[cfg(test)]
    pub(crate) fn panic_after_startup(&self) -> bool {
        self.panic_point == TestPanicPoint::AfterStartup
    }

    #[cfg(not(test))]
    pub(crate) fn panic_after_startup(&self) -> bool {
        false
    }

    /// Returns a deterministic test error before the configured thread spawn.
    pub(crate) fn should_fail_thread_spawn(&self, role: &'static str) -> bool {
        #[cfg(test)]
        {
            let mut failure = self
                .thread_spawn_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(config) = failure.as_mut() else {
                return false;
            };
            if config.role != role {
                return false;
            }
            if config.successful_spawns_before_failure == 0 {
                *failure = None;
                return true;
            }
            config.successful_spawns_before_failure -= 1;
        }
        #[cfg(not(test))]
        let _ = role;
        false
    }

    pub(crate) fn worker_stopped(&self, started: bool) {
        if started {
            self.live_workers
                .fetch_sub(1, Ordering::AcqRel);
        }
        self.record_health_metrics();
    }

    pub(crate) fn report_worker_failure(&self, worker_id: usize, reason: WorkerFailureReason) {
        let mut fatal_failure = self
            .fatal_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if fatal_failure.is_some() {
            return;
        }

        let failure = WorkerFailure {
            pool: self.pool.clone(),
            algorithm: self.algorithm.clone(),
            worker_id,
            reason,
        };
        // Explicit worker exit classification, rather than a later shutdown-flag read, determines
        // whether this is fatal. Make health fail closed before publishing the first failure.
        self.shutdown_requested
            .store(true, Ordering::Release);
        *fatal_failure = Some(failure.clone());
        drop(fatal_failure);

        // Persist fatal state before waking sibling workers or the RPC owner.
        let _ = self.shutdown_tx.send(());
        self.record_health_metrics();

        let reason = failure.reason.metric_label();
        error!(
            pool = %failure.pool,
            algorithm = %failure.algorithm,
            worker_id = failure.worker_id,
            reason,
            failure = ?failure.reason,
            "worker stopped unexpectedly; requesting process-fatal shutdown"
        );
        metrics::counter!(
            "worker_pool_worker_exits_total",
            "pool" => failure.pool.clone(),
            "reason" => reason
        )
        .increment(1);
        let _ = self.failure_tx.send(failure);
    }

    fn fatal_failure(&self) -> Option<WorkerFailure> {
        self.fatal_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record_health_metrics(&self) {
        metrics::gauge!("worker_pool_configured_workers", "pool" => self.pool.clone())
            .set(self.configured_workers as f64);
        metrics::gauge!("worker_pool_live_workers", "pool" => self.pool.clone()).set(
            self.live_workers
                .load(Ordering::Acquire) as f64,
        );
    }
}

/// Cloneable health view for a worker pool.
#[derive(Clone)]
pub struct HealthHandle {
    state: Arc<PoolState>,
}

impl HealthHandle {
    /// Returns the configured worker count.
    pub fn configured_workers(&self) -> usize {
        self.state.configured_workers
    }

    /// Returns workers that completed startup and have not yet exited.
    pub fn live_workers(&self) -> usize {
        self.state
            .live_workers
            .load(Ordering::Acquire)
    }

    /// Returns `true` only when every configured worker is live and the pool has not stopped.
    pub fn is_healthy(&self) -> bool {
        self.configured_workers() > 0 &&
            !self.state.shutdown_requested() &&
            self.state.fatal_failure().is_none() &&
            self.live_workers() == self.configured_workers()
    }
}

/// A pool of worker threads for processing solve tasks.
pub struct WorkerPool {
    name: String,
    algorithm: String,
    state: Arc<PoolState>,
    /// The supervisor owns and joins every worker handle.
    supervisor: Option<JoinHandle<()>>,
    // Keeps the Fynd router queue open until pool shutdown is durable. Direct `spawn` callers
    // keep their existing receiver-closure semantics.
    _task_queue_guard: Option<TaskQueueHandle>,
    // Keeps Fynd-built market receivers open until pool shutdown is durable. Direct builders keep
    // channel closure as an unexpected worker exit.
    _market_event_guard: Option<broadcast::Sender<MarketEvent>>,
}

struct WorkerTaskQueue {
    receiver: async_channel::Receiver<SolveTask>,
    guard: Option<TaskQueueHandle>,
}

impl WorkerPool {
    /// Spawns a worker pool and its supervisor.
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm is unknown or a worker/supervisor thread cannot spawn.
    pub fn spawn(
        config: WorkerPoolConfig,
        task_rx: async_channel::Receiver<SolveTask>,
        market_data: MarketData,
        derived_data: SharedDerivedDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
        derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    ) -> Result<Self, WorkerPoolSpawnError> {
        Self::spawn_inner(
            config,
            WorkerTaskQueue { receiver: task_rx, guard: None },
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            None,
        )
    }

    fn spawn_inner(
        config: WorkerPoolConfig,
        task_queue: WorkerTaskQueue,
        market_data: MarketData,
        derived_data: SharedDerivedDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
        derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
        market_event_guard: Option<broadcast::Sender<MarketEvent>>,
    ) -> Result<Self, WorkerPoolSpawnError> {
        if config.num_workers == 0 {
            return Err(WorkerPoolSpawnError::ZeroWorkers);
        }
        let (shutdown_tx, _) = broadcast::channel(1);
        let (failure_tx, _) = broadcast::channel(1);
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let name = config.name.clone();
        let algorithm = config
            .spawner
            .algorithm_name()
            .to_string();
        let state = Arc::new(PoolState::new(
            name.clone(),
            algorithm.clone(),
            config.num_workers,
            shutdown_tx.clone(),
            failure_tx,
            #[cfg(test)]
            config.thread_spawn_failure,
            #[cfg(test)]
            config.panic_point,
        ));
        state.record_health_metrics();

        let params = SpawnWorkersParams {
            algorithm: algorithm.clone(),
            pool_name: name.clone(),
            num_workers: config.num_workers,
            algorithm_config: config.algorithm_config,
            task_rx: task_queue.receiver,
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            shutdown_tx,
            liquidity_scope: config.liquidity_scope,
            fallback_fee_tiers: config.fallback_fee_tiers,
            state: Arc::clone(&state),
            exit_tx,
        };
        let workers = config.spawner.spawn(params)?;
        let worker_handles = Arc::new(Mutex::new(Some(workers)));
        let handles_for_supervisor = Arc::clone(&worker_handles);
        let configured_workers = state.configured_workers;
        let supervisor = match spawn_thread(
            &state,
            "worker supervisor",
            std::thread::Builder::new().name(format!("{algorithm}-worker-supervisor")),
            move || monitor_workers(configured_workers, exit_rx, handles_for_supervisor),
        ) {
            Ok(supervisor) => supervisor,
            Err(source) => {
                state.request_shutdown();
                join_worker_handles(&worker_handles);
                return Err(WorkerPoolSpawnError::ThreadSpawn { role: "worker supervisor", source });
            }
        };

        info!(name = %name, algorithm = %algorithm, num_workers = config.num_workers, "worker pool spawned");
        Ok(Self {
            name,
            algorithm,
            state,
            supervisor: Some(supervisor),
            _task_queue_guard: task_queue.guard,
            _market_event_guard: market_event_guard,
        })
    }

    /// Returns the worker pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the algorithm name for this worker pool.
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the configured number of workers.
    pub fn num_workers(&self) -> usize {
        self.state.configured_workers
    }

    /// Returns a cloneable health handle for service health checks.
    pub fn health_handle(&self) -> HealthHandle {
        HealthHandle { state: Arc::clone(&self.state) }
    }

    /// Returns `true` only when every configured worker has started and no fatal failure exists.
    pub fn is_healthy(&self) -> bool {
        self.health_handle().is_healthy()
    }

    /// Returns the first process-fatal worker failure, if one has been detected.
    pub fn fatal_failure(&self) -> Option<WorkerFailure> {
        self.state.fatal_failure()
    }

    /// Subscribes to process-fatal worker failures.
    pub fn subscribe_failures(&self) -> broadcast::Receiver<WorkerFailure> {
        self.state.failure_tx.subscribe()
    }

    /// Waits for the first process-fatal worker failure.
    pub async fn wait_for_failure(&self) -> Result<WorkerFailure, broadcast::error::RecvError> {
        let mut failures = self.subscribe_failures();
        if let Some(failure) = self.fatal_failure() {
            return Ok(failure);
        }
        failures.recv().await
    }

    /// Shuts down workers and joins the supervisor. Safe to call after a fatal failure.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    /// Shuts down workers, joins the supervisor, and returns any failure claimed first.
    pub fn shutdown_with_failure(mut self) -> Option<WorkerFailure> {
        self.stop_and_join();
        self.state.fatal_failure()
    }

    fn stop_and_join(&mut self) {
        self.state.request_shutdown();
        if let Some(supervisor) = self.supervisor.take() {
            if let Err(panic) = supervisor.join() {
                error!(name = %self.name, panic = ?safe_panic_text(panic), "worker supervisor panicked");
            }
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

pub(crate) fn spawn_thread<T>(
    state: &PoolState,
    role: &'static str,
    builder: std::thread::Builder,
    task: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<JoinHandle<T>>
where
    T: Send + 'static,
{
    if state.should_fail_thread_spawn(role) {
        return Err(std::io::Error::other("injected thread spawn failure"));
    }
    builder.spawn(task)
}

fn monitor_workers(
    configured_workers: usize,
    exit_rx: std::sync::mpsc::Receiver<()>,
    worker_handles: Arc<Mutex<Option<Vec<JoinHandle<()>>>>>,
) {
    for _ in 0..configured_workers {
        let Ok(()) = exit_rx.recv() else {
            break;
        };
    }
    join_worker_handles(&worker_handles);
}

fn join_worker_handles(worker_handles: &Arc<Mutex<Option<Vec<JoinHandle<()>>>>>) {
    let handles = worker_handles
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .unwrap_or_default();
    for handle in handles {
        if let Err(panic) = handle.join() {
            error!(panic = ?safe_panic_text(panic), "worker wrapper panicked");
        }
    }
}

pub(crate) fn safe_panic_text(panic: Box<dyn Any + Send + 'static>) -> String {
    let message = if let Some(message) = panic.downcast_ref::<&str>() {
        *message
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.as_str()
    } else {
        return "non-string panic payload".to_owned();
    };

    message
        .chars()
        .take(1_024)
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect()
}

/// Builder for [`WorkerPool`].
#[must_use = "a builder does nothing until .build() is called"]
pub struct WorkerPoolBuilder {
    config: WorkerPoolConfig,
    market_event_guard: Option<broadcast::Sender<MarketEvent>>,
}

impl WorkerPoolBuilder {
    /// Creates a builder with default configuration values.
    pub fn new() -> Self {
        Self { config: WorkerPoolConfig::default(), market_event_guard: None }
    }

    /// Sets the worker pool name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Sets the built-in algorithm name.
    pub fn algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.config.spawner = AlgorithmSpawner::Registry { algorithm: algorithm.into() };
        self
    }

    /// Sets a custom algorithm implementation via a factory closure.
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

    /// Sets the PropAMMRouter fee tiers this pool's workers read.
    pub fn fallback_fee_tiers(mut self, fallback_fee_tiers: SharedFeeTiers) -> Self {
        self.config.fallback_fee_tiers = fallback_fee_tiers;
        self
    }

    pub(crate) fn market_event_guard(
        mut self,
        market_event_guard: broadcast::Sender<MarketEvent>,
    ) -> Self {
        self.market_event_guard = Some(market_event_guard);
        self
    }

    #[cfg(test)]
    fn fail_thread_spawn_for_test(
        mut self,
        role: &'static str,
        successful_spawns_before_failure: usize,
    ) -> Self {
        self.config.thread_spawn_failure =
            Some(TestThreadSpawnFailure { role, successful_spawns_before_failure });
        self
    }

    #[cfg(test)]
    fn panic_during_initialization_for_test(mut self) -> Self {
        self.config.panic_point = TestPanicPoint::DuringInitialization;
        self
    }

    #[cfg(test)]
    fn panic_after_startup_for_test(mut self) -> Self {
        self.config.panic_point = TestPanicPoint::AfterStartup;
        self
    }

    /// Builds and starts a pool and returns its task-queue handle.
    pub fn build(
        self,
        market_data: MarketData,
        derived_data: SharedDerivedDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
        derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
    ) -> Result<(WorkerPool, TaskQueueHandle), WorkerPoolSpawnError> {
        let task_queue =
            TaskQueue::new(TaskQueueConfig { capacity: self.config.task_queue_capacity });
        let (task_handle, task_rx) = task_queue.split();
        let pool = WorkerPool::spawn_inner(
            self.config,
            WorkerTaskQueue { receiver: task_rx, guard: Some(task_handle.clone()) },
            market_data,
            derived_data,
            event_rx,
            derived_event_rx,
            self.market_event_guard,
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
        sync::{mpsc, Arc, Mutex},
        time::Duration,
    };

    use tokio::sync::broadcast;

    use super::*;
    use crate::{derived::DerivedData, feed::market_data::MarketData};

    #[tokio::test]
    async fn test_panicking_worker_marks_pool_unhealthy_and_reports_failure() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (market_tx, market_rx) = broadcast::channel(1);
        let (derived_tx, derived_rx) = broadcast::channel(1);
        let (constructor_started_tx, constructor_started_rx) = mpsc::channel();

        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("panic_pool")
            .with_algorithm("panic_algorithm", move |_config| -> crate::MostLiquidAlgorithm {
                constructor_started_tx.send(()).unwrap();
                panic!("intentional worker panic")
            })
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();

        constructor_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker constructor must start before it panics");
        let failure = tokio::time::timeout(Duration::from_secs(1), pool.wait_for_failure())
            .await
            .expect("worker failure must be detected")
            .expect("worker failure channel must remain open");

        assert_eq!(failure.pool(), "panic_pool");
        assert_eq!(failure.algorithm(), "panic_algorithm");
        assert_eq!(failure.worker_id(), 0);
        assert!(matches!(failure.reason(), WorkerFailureReason::Panic(_)));
        assert!(!pool.is_healthy());

        pool.shutdown();
        drop((market_tx, derived_tx));
    }

    #[tokio::test]
    async fn test_closed_task_queue_reports_ordinary_worker_exit() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (market_tx, market_rx) = broadcast::channel(1);
        let (derived_tx, derived_rx) = broadcast::channel(1);
        let task_queue = TaskQueue::new(TaskQueueConfig { capacity: 1 });
        let (tasks, task_rx) = task_queue.split();
        let pool = WorkerPool::spawn(
            WorkerPoolConfig {
                name: "ordinary_exit_pool".to_string(),
                num_workers: 1,
                ..WorkerPoolConfig::default()
            },
            task_rx,
            market_data,
            derived_data,
            market_rx,
            derived_rx,
        )
        .unwrap();

        drop(tasks);
        let failure = tokio::time::timeout(Duration::from_secs(1), pool.wait_for_failure())
            .await
            .expect("ordinary worker exit must be detected")
            .expect("worker failure channel must remain open");

        assert_eq!(failure.pool(), "ordinary_exit_pool");
        assert!(matches!(failure.reason(), WorkerFailureReason::Returned));
        assert!(!pool.is_healthy());
        pool.shutdown();
        drop((market_tx, derived_tx));
    }

    #[tokio::test]
    async fn test_dropping_router_task_handle_before_pool_shutdown_is_not_fatal() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, tasks) = WorkerPoolBuilder::new()
            .name("guarded_queue_pool")
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();
        let health = pool.health_handle();
        let mut failures = pool.subscribe_failures();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must start before the task handle is dropped");

        drop(tasks);
        assert!(tokio::time::timeout(Duration::from_millis(100), pool.wait_for_failure())
            .await
            .is_err());
        assert!(pool.is_healthy());

        pool.shutdown();
        assert!(matches!(failures.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert!(!health.is_healthy());
    }

    #[tokio::test]
    async fn test_fynd_market_guard_prevents_producer_abort_from_becoming_fatal() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let market_event_guard = market_tx.clone();
        let producer = tokio::spawn(async move {
            let _market_tx = market_tx;
            std::future::pending::<()>().await;
        });
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("guarded_market_pool")
            .num_workers(1)
            .market_event_guard(market_event_guard)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();
        let health = pool.health_handle();
        let mut failures = pool.subscribe_failures();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must start before the producer is aborted");

        producer.abort();
        let _ = producer.await;
        assert!(tokio::time::timeout(Duration::from_millis(100), pool.wait_for_failure())
            .await
            .is_err());
        assert!(pool.is_healthy());

        pool.shutdown();
        assert!(matches!(failures.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert!(!health.is_healthy());
    }

    #[tokio::test]
    async fn test_direct_market_channel_closure_remains_fatal() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("unguarded_market_pool")
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();
        let health = pool.health_handle();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must start before the market channel is closed");

        drop(market_tx);
        let failure = tokio::time::timeout(Duration::from_secs(1), pool.wait_for_failure())
            .await
            .expect("unguarded market closure must be detected")
            .expect("worker failure channel must remain open");
        assert_eq!(failure.pool(), "unguarded_market_pool");
        assert_eq!(failure.reason(), &WorkerFailureReason::Returned);
        assert!(!pool.is_healthy());
        pool.shutdown();
    }

    #[test]
    fn test_drop_requests_shutdown_before_joining_supervisor() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("drop_pool")
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();
        let health = pool.health_handle();

        drop(pool);

        assert!(!health.is_healthy());
    }

    #[test]
    fn test_shutdown_returns_failure_claimed_before_cleanup() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("late_failure_pool")
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();

        pool.state
            .report_worker_failure(7, WorkerFailureReason::Returned);
        let failure = pool
            .shutdown_with_failure()
            .expect("a failure claimed before cleanup must remain process-fatal");
        assert_eq!(failure.worker_id(), 7);
        assert_eq!(failure.reason(), &WorkerFailureReason::Returned);
    }

    #[tokio::test]
    async fn test_expected_shutdown_does_not_report_a_worker_failure() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("expected_shutdown_pool")
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();
        let health = pool.health_handle();
        let mut failures = pool.subscribe_failures();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must start before the shutdown assertion");

        pool.shutdown();
        assert!(matches!(failures.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert!(!health.is_healthy());
    }

    #[tokio::test]
    async fn test_initialization_panic_is_process_fatal_and_never_healthy() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("initialization_panic_pool")
            .panic_during_initialization_for_test()
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();

        let failure = tokio::time::timeout(Duration::from_secs(1), pool.wait_for_failure())
            .await
            .expect("initialization panic must be observed")
            .expect("failure channel must remain open");
        assert!(
            matches!(failure.reason(), WorkerFailureReason::Panic(message) if message.contains("initialization"))
        );
        assert_eq!(pool.health_handle().live_workers(), 0);
        assert!(!pool.is_healthy());
        pool.shutdown();
    }

    #[tokio::test]
    async fn test_post_start_panic_decrements_live_worker_count() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (pool, _tasks) = WorkerPoolBuilder::new()
            .name("post_start_panic_pool")
            .panic_after_startup_for_test()
            .num_workers(1)
            .build(market_data, derived_data, market_rx, derived_rx)
            .unwrap();

        let failure = tokio::time::timeout(Duration::from_secs(1), pool.wait_for_failure())
            .await
            .expect("post-start panic must be observed")
            .expect("failure channel must remain open");
        assert!(
            matches!(failure.reason(), WorkerFailureReason::Panic(message) if message.contains("after startup"))
        );
        assert_eq!(pool.health_handle().live_workers(), 0);
        assert!(!pool.is_healthy());
        pool.shutdown();
    }

    #[test]
    fn test_injected_worker_spawn_failure_joins_already_started_workers() {
        let (factory_started_tx, factory_started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let factory = {
            let release_rx = Arc::clone(&release_rx);
            move |config| {
                factory_started_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv()
                    .unwrap();
                crate::MostLiquidAlgorithm::with_config(config).unwrap()
            }
        };
        let builder = WorkerPoolBuilder::new()
            .with_algorithm("blocked_factory", factory)
            .num_workers(2)
            .fail_thread_spawn_for_test("worker", 1);
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (result_tx, result_rx) = mpsc::channel();
        let build_thread = std::thread::spawn(move || {
            result_tx
                .send(builder.build(market_data, derived_data, market_rx, derived_rx))
                .unwrap();
        });

        factory_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the first worker must start before the injected second spawn failure");
        release_tx.send(()).unwrap();
        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("partial worker cleanup must complete");
        assert!(matches!(result, Err(WorkerPoolSpawnError::ThreadSpawn { role: "worker", .. })));
        build_thread.join().unwrap();
    }

    #[test]
    fn test_injected_supervisor_spawn_failure_joins_workers() {
        let (factory_started_tx, factory_started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let factory = {
            let release_rx = Arc::clone(&release_rx);
            move |config| {
                factory_started_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv()
                    .unwrap();
                crate::MostLiquidAlgorithm::with_config(config).unwrap()
            }
        };
        let builder = WorkerPoolBuilder::new()
            .with_algorithm("blocked_factory", factory)
            .num_workers(1)
            .fail_thread_spawn_for_test("worker supervisor", 0);
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);
        let (result_tx, result_rx) = mpsc::channel();
        let build_thread = std::thread::spawn(move || {
            result_tx
                .send(builder.build(market_data, derived_data, market_rx, derived_rx))
                .unwrap();
        });

        factory_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker must start before the injected supervisor spawn failure");
        release_tx.send(()).unwrap();
        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("supervisor failure must join the worker before returning");
        assert!(matches!(
            result,
            Err(WorkerPoolSpawnError::ThreadSpawn { role: "worker supervisor", .. })
        ));
        build_thread.join().unwrap();
    }

    #[test]
    fn test_zero_workers_is_rejected_before_pool_starts() {
        let market_data = MarketData::new_shared();
        let derived_data = Arc::new(tokio::sync::RwLock::new(DerivedData::new()));
        let (_market_tx, market_rx) = broadcast::channel(1);
        let (_derived_tx, derived_rx) = broadcast::channel(1);

        let result = WorkerPoolBuilder::new()
            .num_workers(0)
            .build(market_data, derived_data, market_rx, derived_rx);

        assert!(matches!(result, Err(WorkerPoolSpawnError::ZeroWorkers)));
    }
}
