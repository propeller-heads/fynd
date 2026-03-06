//! A Solver Worker that processes solve requests and maintains market graph state.
//!
//! The Solver Worker:
//! - Initializes graph from market topology (via a GraphManager)
//! - Consumes MarketEvents to keep local topology in sync
//! - Processes solve requests
//! - Uses an Algorithm to find routes through the market graph
//! - Coordinates market event and solve task processing

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use num_bigint::BigUint;
use tokio::sync::{broadcast, Notify};
use tracing::{debug, error, info, warn};

use crate::{
    algorithm::Algorithm,
    derived::{
        computation::DerivedComputation, computations::PoolDepthComputation,
        events::DerivedDataEvent, tracker::ReadinessTracker, SharedDerivedDataRef,
    },
    feed::{
        events::{MarketEvent, MarketEventHandler},
        market_data::SharedMarketDataRef,
    },
    graph::{EdgeWeightUpdaterWithDerived, GraphManager},
    types::internal::SolveTask,
    BlockInfo, Order, OrderQuote, QuoteStatus, SingleOrderQuote, SolveError,
};

/// A solver worker instance that maintains a market graph and processes solve requests.
pub(crate) struct SolverWorker<A>
where
    A: Algorithm,
    A::GraphManager: MarketEventHandler,
{
    /// Algorithm used for route finding.
    algorithm: A,
    /// Graph manager that maintains the graph.
    graph_manager: A::GraphManager,
    /// Reference to shared market data.
    market_data: SharedMarketDataRef,
    /// Reference to shared derived data (pool depths, token prices).
    derived_data: SharedDerivedDataRef,
    /// Tracks readiness of required derived data computations.
    readiness_tracker: ReadinessTracker,
    /// Notified when readiness state may have changed.
    ready_notify: Arc<Notify>,
    /// Whether the graph has been initialized.
    initialized: bool,
    /// Worker identifier (for logging).
    // TODO: make this a string to include pool name
    worker_id: usize,
}

impl<A> SolverWorker<A>
where
    A: Algorithm,
    A::GraphManager: MarketEventHandler,
{
    /// Creates a new Solver.
    ///
    /// The graph manager is automatically created from the algorithm's associated type.
    ///
    /// # Arguments
    ///
    /// * `market_data` - Shared reference to market data
    /// * `derived_data` - Shared reference to derived data (pool depths, token prices)
    /// * `algorithm` - The algorithm to use for route finding
    /// * `worker_id` - Identifier for this worker (for logging)
    pub fn new(
        market_data: SharedMarketDataRef,
        derived_data: SharedDerivedDataRef,
        algorithm: A,
        worker_id: usize,
    ) -> Self {
        let requirements = algorithm.computation_requirements();
        Self {
            algorithm,
            graph_manager: A::GraphManager::default(),
            market_data,
            derived_data,
            readiness_tracker: ReadinessTracker::new(requirements),
            ready_notify: Arc::new(Notify::new()),
            initialized: false,
            worker_id,
        }
    }

    /// Initializes the graph from SharedMarketData.
    ///
    /// Call this on startup or to recreate the graph from the latest market topology.
    /// Gets the market topology from SharedMarketData and uses it to build the graph.
    pub async fn initialize_graph(&mut self) {
        let topology = {
            // read lock on market data
            let market = self.market_data.read().await;
            market.component_topology().clone() // clone to avoid holding the lock
        };

        self.graph_manager
            .initialize_graph(&topology);
        self.initialized = true;
    }

    /// Processes a single market event.
    pub async fn process_event(&mut self, event: MarketEvent) {
        match event {
            MarketEvent::MarketUpdated { .. } => {
                if let Err(e) = self
                    .graph_manager
                    .handle_event(&event)
                    .await
                {
                    // Graph errors currently returned by handle_event are non-fatal, so we just log
                    // them.
                    warn!("Error handling market event: {:?}", e);
                }
            }
        }
    }

    /// Returns a quote for an order.
    pub async fn quote(&mut self, order: &Order) -> Result<SingleOrderQuote, SolveError> {
        let start_time = Instant::now();

        // Log order details once at entry
        debug!(
            order_id = %order.id(),
            token_in = ?order.token_in(),
            token_out = ?order.token_out(),
            amount = %order.amount(),
            side = ?order.side(),
            "processing order"
        );

        // Check readiness before solving
        if self
            .readiness_tracker
            .has_requirements() &&
            !self.readiness_tracker.is_ready()
        {
            return Err(SolveError::NotReady(format!(
                "derived data not ready: missing {:?}",
                self.readiness_tracker.missing()
            )));
        }

        // Ensure we're initialized
        if !self.initialized {
            self.initialize_graph().await;
        }

        // Get the graph from the graph manager
        let graph = self.graph_manager.graph();

        // Get block info
        // TODO: maybe the algorithm should return the block info with the route? The block might
        // update while solving and the route returned might be for the newer block.
        let block_info = {
            let market = self.market_data.read().await;
            let last_block = market
                .last_updated()
                .ok_or(SolveError::NotReady("No block info".to_string()))?;
            BlockInfo::new(
                last_block.number(),
                format!("{:?}", last_block.hash()),
                last_block.timestamp(),
            )
        };

        let result = self
            .algorithm
            .find_best_route(
                graph,
                self.market_data.clone(),
                Some(self.derived_data.clone()),
                order,
            )
            .await;

        let order_quote = match result {
            Ok(result) => {
                // Extract scalar values before consuming result with into_route()
                let amount_out_net_gas = result
                    .net_amount_out()
                    .to_biguint()
                    .unwrap_or(BigUint::ZERO);
                let gas_price = result.gas_price().clone();
                let route = result.into_route();

                let gas_estimate = route.total_gas();
                let amount_in = if order.is_sell() {
                    order.amount().clone()
                } else {
                    route
                        .swaps()
                        .first()
                        .map(|s| s.amount_in().clone())
                        .ok_or_else(|| {
                            error!(
                                order_id = %order.id(),
                                "route missing first swap for buy order"
                            );
                            SolveError::NoRouteFound { order_id: order.id().to_string() }
                        })?
                };
                let amount_out = if order.is_sell() {
                    route
                        .swaps()
                        .last()
                        .map(|s| s.amount_out().clone())
                        .ok_or_else(|| {
                            error!(
                                order_id = %order.id(),
                                "route missing last swap for sell order"
                            );
                            SolveError::NoRouteFound { order_id: order.id().to_string() }
                        })?
                } else {
                    order.amount().clone()
                };

                OrderQuote::new(
                    order.id().to_string(),
                    QuoteStatus::Success,
                    amount_in,
                    amount_out,
                    gas_estimate,
                    amount_out_net_gas,
                    block_info.clone(),
                    self.algorithm.name().to_string(),
                )
                .with_route(route)
                .with_gas_price(gas_price)
            }
            Err(err) => {
                let solve_error = match err {
                    crate::AlgorithmError::NoPath { .. } => {
                        error!(
                            order_id = %order.id(),
                            error = %err,
                            "no route found"
                        );
                        SolveError::NoRouteFound { order_id: order.id().to_string() }
                    }
                    crate::AlgorithmError::Timeout { elapsed_ms } => {
                        error!(
                            order_id = %order.id(),
                            elapsed_ms,
                            "solve timeout"
                        );
                        SolveError::Timeout { elapsed_ms }
                    }
                    _ => {
                        error!(
                            order_id = %order.id(),
                            error = %err,
                            "algorithm error"
                        );
                        SolveError::AlgorithmError(err.to_string())
                    }
                };
                return Err(solve_error);
            }
        };

        let solve_time_ms = start_time.elapsed().as_millis() as u64;

        Ok(SingleOrderQuote::new(order_quote, solve_time_ms))
    }

    /// Waits for required derived data to become ready, or until timeout.
    ///
    /// Uses a Notify pattern to know when it's available to solve.
    ///
    /// Returns `Ok(())` if ready or no requirements, `Err` if timeout reached.
    async fn wait_until_ready(&self, timeout: Duration) -> Result<(), SolveError> {
        // Fast path: no requirements or already ready
        if !self
            .readiness_tracker
            .has_requirements() ||
            self.readiness_tracker.is_ready()
        {
            return Ok(());
        }

        let deadline = Instant::now() + timeout;

        loop {
            // Create notified future BEFORE checking state (important for race-free waiting)
            let notified = self.ready_notify.notified();

            // Check if ready
            if self.readiness_tracker.is_ready() {
                return Ok(());
            }

            // Calculate remaining time
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SolveError::NotReady(format!(
                    "timeout waiting for derived data: missing {:?}",
                    self.readiness_tracker.missing()
                )));
            }

            // Wait for notification or timeout
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    return Err(SolveError::NotReady(format!(
                        "timeout waiting for derived data: missing {:?}",
                        self.readiness_tracker.missing()
                    )));
                }
                _ = notified => {
                    // Woken up by notify, loop to check readiness again
                    continue;
                }
            }
        }
    }

    /// Runs the worker's main loop, processing market events and solve tasks.
    ///
    /// This method coordinates between market events and solve requests, ensuring the graph
    /// stays up-to-date while processing solve tasks.
    ///
    /// # Arguments
    ///
    /// * `event_rx` - Receiver for market events
    /// * `derived_event_rx` - Receiver for derived data events (pool depths, etc.)
    /// * `task_rx` - Shared receiver for solve tasks
    /// * `shutdown_rx` - Receiver for shutdown signals
    pub async fn run(
        &mut self,
        mut event_rx: broadcast::Receiver<MarketEvent>,
        mut derived_event_rx: broadcast::Receiver<DerivedDataEvent>,
        task_rx: async_channel::Receiver<SolveTask>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) where
        A::GraphManager: EdgeWeightUpdaterWithDerived,
    {
        info!(self.worker_id, "worker started");

        loop {
            tokio::select! {
                biased; // prioritize events in this order: shutdown, market update, derived data, solve task

                // Check for shutdown
                _ = shutdown_rx.recv() => {
                    info!(self.worker_id, "worker shutting down");
                    break;
                }

                // Process market events
                event_result = event_rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            self.process_event(event).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!(self.worker_id, "event receiver closed, shutting down");
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                self.worker_id,
                                skipped = skipped,
                                "event receiver lagged, skipped {} events. Reinitializing graph from current market state",
                                skipped
                            );
                            // Reinitialize the graph from the current market state to recover from the missed events.
                            self.initialize_graph().await;
                        }
                    }
                }

                // Process derived data events (pool depths, token prices)
                derived_result = derived_event_rx.recv() => {
                    match derived_result {
                        Ok(event) => {
                            // Always update tracker with every event
                            self.readiness_tracker.handle_event(&event);

                            // Signal waiters that readiness may have changed
                            self.ready_notify.notify_waiters();

                            // TODO: This handling breaks the worker abstraction, assuming that weights
                            // will always be used, and they will always come from PoolDepth. A refactor
                            // is needed to move this handling to the algorithm.
                            if let DerivedDataEvent::ComputationComplete { computation_id, block } = &event {
                                if *computation_id == PoolDepthComputation::ID {
                                    let market = self.market_data.read().await;
                                    let derived = self.derived_data.read().await;
                                    let updated = self.graph_manager.update_edge_weights_with_derived(&market, &derived);
                                    debug!(
                                        self.worker_id,
                                        block,
                                        updated,
                                        "updated edge weights with derived data"
                                    );
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            warn!(self.worker_id, "derived event receiver closed");
                            // Continue running - derived data won't update but we can still solve
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                self.worker_id,
                                skipped,
                                "derived event receiver lagged, skipped {} events",
                                skipped
                            );
                            // Try to update with current derived data
                            let market = self.market_data.read().await;
                            let derived = self.derived_data.read().await;
                            if derived.pool_depths().is_some() {
                                let updated = self.graph_manager.update_edge_weights_with_derived(&market, &derived);
                                debug!(
                                    self.worker_id,
                                    updated,
                                    "recovered edge weights after lag"
                                );
                            }
                        }
                    }
                }

                // Get next solve task
                task = task_rx.recv() => {
                    match task.ok() {
                        Some(task) => {
                            let task_id = task.id();
                            let _wait_time = task.wait_time();

                            // Wait for derived data readiness before solving
                            // Use algorithm timeout as the max wait time
                            if let Err(e) = self.wait_until_ready(self.algorithm.timeout()).await {
                                warn!(
                                    self.worker_id,
                                    task_id = %task_id,
                                    error = %e,
                                    "not ready to solve"
                                );
                                task.respond(Err(e));
                                continue;
                            }

                            // Process the task
                            let result = {
                                let order = task.order();
                                self.quote(order).await
                            };

                            if let Err(ref e) = result {
                                warn!(
                                    self.worker_id,
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
                            info!(self.worker_id, "task channel closed, exiting");
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        algorithm::{most_liquid::DepthAndPrice, test_utils::setup_market},
        derived::{
            computation::ComputationRequirements,
            computations::{SpotPriceComputation, TokenGasPriceComputation},
            DerivedData,
        },
        graph::petgraph::{PetgraphStableDiGraphManager, StableDiGraph},
    };

    /// A minimal mock algorithm for testing the worker.
    /// Uses DepthAndPrice as the edge weight type to satisfy trait bounds.
    struct MockAlgorithm {
        requirements: ComputationRequirements,
        timeout: Duration,
    }

    impl MockAlgorithm {
        fn new() -> Self {
            Self { requirements: ComputationRequirements::none(), timeout: Duration::from_secs(1) }
        }

        fn with_requirements(mut self, requirements: ComputationRequirements) -> Self {
            self.requirements = requirements;
            self
        }
    }

    impl Algorithm for MockAlgorithm {
        type GraphType = StableDiGraph<DepthAndPrice>;
        type GraphManager = PetgraphStableDiGraphManager<DepthAndPrice>;

        fn name(&self) -> &str {
            "mock"
        }

        async fn find_best_route(
            &self,
            _graph: &Self::GraphType,
            _market: SharedMarketDataRef,
            _derived: Option<SharedDerivedDataRef>,
            _order: &Order,
        ) -> Result<crate::types::RouteResult, crate::AlgorithmError> {
            Err(crate::AlgorithmError::Other("not implemented".to_string()))
        }

        fn computation_requirements(&self) -> ComputationRequirements {
            self.requirements.clone()
        }

        fn timeout(&self) -> Duration {
            self.timeout
        }
    }

    // ==================== wait_until_ready Tests ====================

    #[tokio::test]
    async fn wait_until_ready_returns_immediately_when_no_requirements() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let algorithm = MockAlgorithm::new();
        let worker = SolverWorker::new(market, derived, algorithm, 0);

        // Should return immediately since there are no requirements
        let result = worker
            .wait_until_ready(Duration::from_millis(10))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_until_ready_returns_immediately_when_already_ready() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .allow_stale(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0);

        // Mark as ready by handling a completion event
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationComplete {
                computation_id: SpotPriceComputation::ID,
                block: 1,
            });

        // Should return immediately since already ready
        let result = worker
            .wait_until_ready(Duration::from_millis(10))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_until_ready_times_out_when_not_ready() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let worker = SolverWorker::new(market, derived, algorithm, 0);

        // Should timeout since no events are received
        let result = worker
            .wait_until_ready(Duration::from_millis(50))
            .await;

        assert!(result.is_err());
        match result {
            Err(SolveError::NotReady(msg)) => {
                assert!(msg.contains("timeout"));
                assert!(msg.contains("spot_prices"));
            }
            other => panic!("Expected NotReady error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn wait_until_ready_wakes_up_on_notify() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let worker = SolverWorker::new(market, derived, algorithm, 0);

        // Clone the notify handle to simulate the main loop notifying
        let notify = worker.ready_notify.clone();

        // Spawn a task that will notify after a short delay
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            notify.notify_waiters();
        });

        // wait_until_ready should wake up when notified but still timeout
        // because we didn't actually update the tracker
        let result = worker
            .wait_until_ready(Duration::from_millis(100))
            .await;

        handle.await.unwrap();

        // Should still timeout because notify woke us up but we're not actually ready
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn wait_until_ready_succeeds_when_notified_and_ready() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0);

        // Clone the notify handle and get a reference to the tracker
        let notify = worker.ready_notify.clone();

        // Spawn a task that will update tracker and notify
        let handle = tokio::spawn({
            // We need to update the tracker from outside, so we simulate
            // what the main loop does: update tracker then notify
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                notify.notify_waiters();
            }
        });

        // Manually update the tracker to simulate what would happen in the main loop
        // In real usage, the main loop updates tracker THEN notifies
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationComplete {
                computation_id: SpotPriceComputation::ID,
                block: 1,
            });

        // Now wait - should succeed immediately since we're already ready
        let result = worker
            .wait_until_ready(Duration::from_millis(100))
            .await;

        handle.abort(); // Don't need to wait for the spawned task
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn notify_pattern_handles_multiple_waiters() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .allow_stale(TokenGasPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0);

        let notify = worker.ready_notify.clone();

        // Spawn multiple waiting tasks
        let notify1 = notify.clone();
        let waiter1 = tokio::spawn(async move {
            notify1.notified().await;
            true
        });

        let notify2 = notify.clone();
        let waiter2 = tokio::spawn(async move {
            notify2.notified().await;
            true
        });

        // Give waiters time to register
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Update tracker and notify all waiters
        worker
            .readiness_tracker
            .handle_event(&DerivedDataEvent::ComputationComplete {
                computation_id: TokenGasPriceComputation::ID,
                block: 1,
            });
        notify.notify_waiters();

        // Both waiters should complete
        let (r1, r2) = tokio::join!(waiter1, waiter2);
        assert!(r1.unwrap());
        assert!(r2.unwrap());
    }

    // ==================== Integration Tests with run() ====================

    #[tokio::test]
    async fn worker_updates_tracker_and_notifies_on_derived_event() {
        let (market, _) = setup_market(vec![]);
        let derived = DerivedData::new_shared();

        let requirements = ComputationRequirements::none()
            .require_fresh(SpotPriceComputation::ID)
            .unwrap();
        let algorithm = MockAlgorithm::new().with_requirements(requirements);
        let mut worker = SolverWorker::new(market, derived, algorithm, 0);

        // Create channels
        let (_event_tx, event_rx) = broadcast::channel::<MarketEvent>(16);
        let (derived_tx, derived_rx) = broadcast::channel::<DerivedDataEvent>(16);
        let (_task_tx, task_rx) = async_channel::bounded::<crate::types::internal::SolveTask>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        // Spawn worker
        let handle = tokio::spawn(async move {
            worker
                .run(event_rx, derived_rx, task_rx, shutdown_rx)
                .await;
        });

        // Send a derived data event
        derived_tx
            .send(DerivedDataEvent::ComputationComplete {
                computation_id: SpotPriceComputation::ID,
                block: 1,
            })
            .unwrap();

        // Give worker time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Shutdown
        let _ = shutdown_tx.send(());

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("worker should shutdown")
            .expect("worker task should not panic");
    }
}
