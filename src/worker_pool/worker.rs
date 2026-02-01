//! A Solver Worker that processes solve requests and maintains market graph state.
//!
//! The Solver Worker:
//! - Initializes graph from market topology (via a GraphManager)
//! - Consumes MarketEvents to keep local topology in sync
//! - Processes solve requests
//! - Uses an Algorithm to find routes through the market graph
//! - Coordinates market event and solve task processing

use std::time::Instant;

use num_bigint::BigUint;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::{
    algorithm::Algorithm,
    derived::{
        events::DerivedDataEvent, DerivedComputation, PoolDepthComputation, SharedDerivedDataRef,
    },
    feed::{
        events::{MarketEvent, MarketEventHandler},
        market_data::SharedMarketDataRef,
    },
    graph::{EdgeWeightUpdaterWithDepths, GraphManager},
    types::{
        internal::SolveTask, BlockInfo, OrderSolution, SingleOrderSolution, SolutionStatus,
        SolveError,
    },
    Order,
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
        Self {
            algorithm,
            graph_manager: A::GraphManager::default(),
            market_data,
            derived_data,
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

    /// Solves an order and returns the solution.
    pub async fn solve(&mut self, order: &Order) -> Result<SingleOrderSolution, SolveError> {
        let start_time = Instant::now();

        // Log order details once at entry
        debug!(
            order_id = %order.id,
            token_in = ?order.token_in,
            token_out = ?order.token_out,
            amount = %order.amount,
            side = ?order.side,
            "processing order"
        );

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
            BlockInfo {
                number: last_block.number,
                hash: format!("{:?}", last_block.hash),
                timestamp: last_block.timestamp,
            }
        };

        let result = self
            .algorithm
            .find_best_route(graph, self.market_data.clone(), Some(self.derived_data.clone()), order)
            .await;

        let order_solution = match result {
            Ok(result) => {
                let route = result.route;
                let gas_estimate = route.total_gas();
                let amount_in = if order.is_sell() {
                    order.amount.clone()
                } else {
                    route
                        .swaps
                        .first()
                        .map(|s| s.amount_in.clone())
                        .ok_or_else(|| {
                            error!(
                                order_id = %order.id,
                                "route missing first swap for buy order"
                            );
                            SolveError::NoRouteFound { order_id: order.id.clone() }
                        })?
                };
                let amount_out = if order.is_sell() {
                    route
                        .swaps
                        .last()
                        .map(|s| s.amount_out.clone())
                        .ok_or_else(|| {
                            error!(
                                order_id = %order.id,
                                "route missing last swap for sell order"
                            );
                            SolveError::NoRouteFound { order_id: order.id.clone() }
                        })?
                } else {
                    order.amount.clone()
                };

                // Convert net_amount_out (BigInt) to BigUint for amount_out_net_gas.
                // If net_amount_out is negative (gas > output), clamp to zero.
                let amount_out_net_gas = result.net_amount_out
                    .to_biguint()
                    .unwrap_or(BigUint::ZERO);

                OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::Success,
                    route: Some(route),
                    amount_in,
                    amount_out,
                    gas_estimate,
                    price_impact_bps: None, // TODO: Calculate price impact
                    amount_out_net_gas,
                    block: block_info.clone(),
                    algorithm: self.algorithm.name().to_string(),
                }
            }
            Err(err) => {
                let solve_error = match err {
                    crate::AlgorithmError::NoPath { .. } => {
                        error!(
                            order_id = %order.id,
                            error = %err,
                            "no route found"
                        );
                        SolveError::NoRouteFound { order_id: order.id.clone() }
                    }
                    crate::AlgorithmError::Timeout { elapsed_ms } => {
                        error!(
                            order_id = %order.id,
                            elapsed_ms,
                            "solve timeout"
                        );
                        SolveError::Timeout { elapsed_ms }
                    }
                    _ => {
                        error!(
                            order_id = %order.id,
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

        Ok(SingleOrderSolution { order: order_solution, solve_time_ms })
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
        A::GraphManager: EdgeWeightUpdaterWithDepths,
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
                        Ok(DerivedDataEvent::ComputationComplete { computation_id, block }) => {
                            if computation_id == PoolDepthComputation::ID {
                                // Update edge weights with pool depths
                                let market = self.market_data.read().await;
                                let derived = self.derived_data.read().await;
                                if let Some(pool_depths) = derived.pool_depths() {
                                    let updated = self.graph_manager.update_edge_weights_with_depths(&market, pool_depths);
                                    debug!(
                                        self.worker_id,
                                        block,
                                        updated,
                                        "updated edge weights with pool depths"
                                    );
                                }
                            }
                        }
                        Ok(_) => {
                            // Other derived events (NewBlock, AllComplete) - ignore for now
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
                            if let Some(pool_depths) = derived.pool_depths() {
                                let updated = self.graph_manager.update_edge_weights_with_depths(&market, pool_depths);
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
                            let task_id = task.id;
                            let _wait_time = task.wait_time();
                            let task_response_tx = task.response_tx;

                            // Process the task
                            let result = self.solve(&task.order).await;

                            if let Err(ref e) = result {
                                warn!(
                                    self.worker_id,
                                    task_id = %task_id,
                                    error = %e,
                                    "solve failed"
                                );
                            }

                            // Send response
                            let _ = task_response_tx.send(result);
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
