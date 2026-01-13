//! A Solver Worker that processes solve requests.
//!
//! The Solver Worker:
//! - Initializes graph from market topology (via a GraphManager)
//! - Holds a reference to SharedMarketData (for state lookups)
//! - Subscribes to MarketEvents to keep local topology in sync
//! - Uses an Algorithm to find routes

use std::time::{Duration, Instant};

use num_bigint::BigUint;
use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    algorithm::{Algorithm, AlgorithmError},
    feed::{
        events::{MarketEvent, MarketEventHandler},
        market_data::SharedMarketDataRef,
    },
    graph::GraphManager,
    types::{solution::SolutionRequest, OrderSolution, Solution, SolutionStatus, SolveError},
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

/// A solver worker instance that processes solve requests.
///
/// The solver worker initializes the graph on startup from SharedMarketData, and the graph
/// manager maintains the graph and updates it based on market events.
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
    /// Receiver for market events.
    event_rx: broadcast::Receiver<MarketEvent>,
    /// Configuration.
    config: WorkerConfig,
    /// Whether the graph has been initialized.
    initialized: bool,
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
    /// * `event_rx` - Receiver for market events from the indexer
    /// * `algorithm` - The algorithm to use for route finding
    /// * `config` - Solver configuration
    pub fn new(
        market_data: SharedMarketDataRef,
        event_rx: broadcast::Receiver<MarketEvent>,
        algorithm: A,
        config: WorkerConfig,
    ) -> Self {
        Self {
            algorithm,
            graph_manager: A::GraphManager::default(),
            market_data,
            event_rx,
            config,
            initialized: false,
        }
    }

    /// Initializes the graph from SharedMarketData.
    ///
    /// Call this on startup or when recovering from missed events.
    /// Gets the market topology from SharedMarketData and uses it to build the graph.
    pub async fn initialize_graph(&mut self) {
        let market = self.market_data.read().await;
        let topology = market.component_topology();
        self.graph_manager
            .initialize_graph(&topology);
        self.initialized = true;
    }

    /// Processes pending market events.
    ///
    /// Call this periodically or before each solve to stay in sync.
    ///
    /// Errors are logged but do not stop processing of subsequent events.
    pub fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            if let Err(e) = self.graph_manager.handle_event(&event) {
                warn!("Warning: Error handling market event: {:?}", e);
            }
        }
    }

    /// Solves a request and returns the solution.
    ///
    /// This is the main entry point called by worker threads.
    pub async fn solve(&mut self, request: &SolutionRequest) -> Result<Solution, SolveError> {
        let start_time = Instant::now();

        // Process any pending events first
        self.process_events();

        // Ensure we're initialized
        if !self.initialized {
            self.initialize_graph().await;
        }

        // Get a read lock on market data
        let market = self.market_data.read().await;

        // Get block info for the solution
        let block_info = market
            .last_updated()
            .ok_or(SolveError::Internal(
                "No block info available, this means no block has been processed yet".to_string(),
            ))?;

        // Solve each order
        let mut order_solutions = Vec::with_capacity(request.orders.len());

        for order in &request.orders {
            // Validate order
            if let Err(_e) = order.validate() {
                order_solutions.push(OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::NoRouteFound,
                    route: None,
                    amount_in: order.amount.clone(),
                    amount_out: BigUint::ZERO,
                    gas_estimate: BigUint::ZERO,
                    price_impact_bps: None,
                    block: block_info.clone(),
                    algorithm: String::new(),
                });
                continue;
            }

            // Find route using algorithm
            let result = self
                .algorithm
                .find_best_route(&self.graph_manager, &market, order);

            let order_solution = match result {
                Ok(route) => {
                    let gas_estimate = route.total_gas();
                    let amount_in = route
                        .swaps
                        .first()
                        .map(|s| s.amount_in.clone())
                        .unwrap_or_else(|| BigUint::ZERO);
                    let amount_out = route
                        .swaps
                        .last()
                        .map(|s| s.amount_out.clone())
                        .unwrap_or_else(|| BigUint::ZERO);

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
                Err(AlgorithmError::NoPath { .. }) => OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::NoRouteFound,
                    route: None,
                    amount_in: order.amount.clone(),
                    amount_out: BigUint::ZERO,
                    gas_estimate: BigUint::ZERO,
                    price_impact_bps: None,
                    block: block_info.clone(),
                    algorithm: self.algorithm.name().to_string(),
                },
                Err(AlgorithmError::InsufficientLiquidity) => OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::InsufficientLiquidity,
                    route: None,
                    amount_in: order.amount.clone(),
                    amount_out: BigUint::ZERO,
                    gas_estimate: BigUint::ZERO,
                    price_impact_bps: None,
                    block: block_info.clone(),
                    algorithm: self.algorithm.name().to_string(),
                },
                Err(AlgorithmError::Timeout { .. }) => OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::Timeout,
                    route: None,
                    amount_in: order.amount.clone(),
                    amount_out: BigUint::ZERO,
                    gas_estimate: BigUint::ZERO,
                    price_impact_bps: None,
                    block: block_info.clone(),
                    algorithm: self.algorithm.name().to_string(),
                },
                Err(_) => OrderSolution {
                    order_id: order.id.clone(),
                    status: SolutionStatus::NoRouteFound,
                    route: None,
                    amount_in: order.amount.clone(),
                    amount_out: BigUint::ZERO,
                    gas_estimate: BigUint::ZERO,
                    price_impact_bps: None,
                    block: block_info.clone(),
                    algorithm: self.algorithm.name().to_string(),
                },
            };

            order_solutions.push(order_solution);
        }

        // Calculate totals
        let total_gas_estimate = order_solutions
            .iter()
            .map(|o| &o.gas_estimate)
            .fold(BigUint::ZERO, |acc, g| acc + g);

        let solve_time_ms = start_time.elapsed().as_millis() as u64;

        Ok(Solution { orders: order_solutions, total_gas_estimate, solve_time_ms })
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
