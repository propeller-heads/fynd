//! A Solver Worker that processes solve requests and maintains market graph state.
//!
//! The Solver Worker:
//! - Initializes graph from market topology (via a GraphManager)
//! - Consumess MarketEvents to keep local topology in sync
//! - Processes solve requests
//! - Uses an Algorithm to find routes through the market graph
//! - Coordinates market event and solve task processing

use std::time::{Duration, Instant};

use num_bigint::BigUint;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::{
    algorithm::Algorithm,
    feed::{
        events::{MarketEvent, MarketEventHandler},
        market_data::SharedMarketDataRef,
    },
    graph::GraphManager,
    types::{BlockInfo, OrderSolution, SingleOrderSolution, SolutionStatus, SolveError, SolveTask},
    Order,
};

/// Configuration for a Solver instance.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Maximum hops to search.
    pub max_hops: usize,
    /// Timeout for solving.
    pub timeout: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self { max_hops: 3, timeout: Duration::from_millis(100) }
    }
}

/// A solver worker instance that maintains a market graph and processes solve requests.
pub struct SolverWorker<A>
where
    A: Algorithm,
    A::GraphType: Send + Sync,
    A::GraphManager: GraphManager<A::GraphType> + MarketEventHandler,
{
    /// Algorithm used for route finding.
    algorithm: A,
    /// Graph manager that maintains the graph.
    graph_manager: A::GraphManager,
    /// Reference to shared market data.
    market_data: SharedMarketDataRef,
    /// Configuration.
    config: WorkerConfig,
    /// Whether the graph has been initialized.
    initialized: bool,
    /// Worker identifier (for logging).
    worker_id: usize,
}

impl<A> SolverWorker<A>
where
    A: Algorithm,
    A::GraphType: Send + Sync,
    A::GraphManager: GraphManager<A::GraphType> + MarketEventHandler,
{
    /// Creates a new Solver.
    ///
    /// The graph manager is automatically created from the algorithm's associated type.
    ///
    /// # Arguments
    ///
    /// * `market_data` - Shared reference to market data
    /// * `algorithm` - The algorithm to use for route finding
    /// * `config` - Solver configuration
    /// * `worker_id` - Identifier for this worker (for logging)
    pub fn new(
        market_data: SharedMarketDataRef,
        algorithm: A,
        config: WorkerConfig,
        worker_id: usize,
    ) -> Self {
        Self {
            algorithm,
            graph_manager: A::GraphManager::default(),
            market_data,
            config,
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
    pub fn process_event(&mut self, event: MarketEvent) {
        match event {
            MarketEvent::MarketUpdated { .. } => {
                if let Err(e) = self.graph_manager.handle_event(&event) {
                    // Graph errors currently returned by handle_event are non-fatal, so we just log
                    // them.
                    warn!("Error handling market event: {:?}", e);
                }
            }
            MarketEvent::GasPriceUpdated { .. } => {
                unimplemented!("Gas price updates are not supported yet");
            }
        }
    }

    /// Gets block info from market data.
    fn get_block_info(market: &crate::feed::market_data::SharedMarketData) -> BlockInfo {
        let last_block = market.last_updated();
        BlockInfo {
            number: last_block.number,
            hash: format!("{:?}", last_block.hash),
            timestamp: last_block.ts.and_utc().timestamp() as u64,
        }
    }

    /// Solves an order and returns the solution.
    pub async fn solve(&mut self, order: &Order) -> Result<SingleOrderSolution, SolveError> {
        let start_time = Instant::now();

        // Ensure we're initialized
        if !self.initialized {
            self.initialize_graph().await;
        }

        // Get the graph from the graph manager (no lock needed)
        let graph = self.graph_manager.graph();

        // Keep market data lock scope small
        let (block_info, result) = {
            let market = self.market_data.read().await;
            let block_info = Self::get_block_info(&market);
            let result = self
                .algorithm
                .find_best_route(graph, &market, order);
            (block_info, result)
        };

        let order_solution = match result {
            Ok(route) => {
                let gas_estimate = route.total_gas();
                let amount_in = if order.is_sell() {
                    order.amount.clone()
                } else {
                    route
                        .swaps
                        .first()
                        .map(|s| s.amount_in.clone())
                        .unwrap_or_else(|| BigUint::ZERO)
                };
                let amount_out = if order.is_sell() {
                    route
                        .swaps
                        .last()
                        .map(|s| s.amount_out.clone())
                        .unwrap_or_else(|| BigUint::ZERO)
                } else {
                    order.amount.clone()
                };

                OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::Success,
                    route: Some(route),
                    amount_in,
                    amount_out,
                    gas_estimate,
                    price_impact_bps: None, // TODO: Calculate price impact
                    block: block_info.clone(),
                    algorithm: self.algorithm.name().to_string(),
                }
            }
            Err(err) => {
                let status = SolutionStatus::from(err);
                OrderSolution {
                    order_id: order.id.clone(),
                    status,
                    route: None,
                    amount_in: if order.is_sell() { order.amount.clone() } else { BigUint::ZERO },
                    amount_out: if order.is_sell() { BigUint::ZERO } else { order.amount.clone() },
                    gas_estimate: BigUint::ZERO,
                    price_impact_bps: None,
                    block: block_info,
                    algorithm: String::new(),
                }
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
    /// * `task_rx` - Shared receiver for solve tasks
    /// * `shutdown_rx` - Receiver for shutdown signals
    pub async fn run(
        &mut self,
        mut event_rx: broadcast::Receiver<MarketEvent>,
        task_rx: async_channel::Receiver<SolveTask>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        info!(self.worker_id, "worker started");

        loop {
            tokio::select! {
                biased; // prioritize events in this order: shutdown, market update, solve task

                // Check for shutdown
                _ = shutdown_rx.recv() => {
                    info!(self.worker_id, "worker shutting down");
                    break;
                }

                // Process market events
                event_result = event_rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            self.process_event(event);
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

    /// Returns the algorithm name.
    pub fn algorithm_name(&self) -> &str {
        self.algorithm.name()
    }

    /// Returns the config.
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }
}
