//! Solver component that processes solve requests.
//!
//! Each worker thread owns a Solver instance. The solver:
//! - Owns a local copy of the pool topology (HashMap<PoolId, Vec<Address>>)
//! - Holds a reference to SharedMarketData (for state lookups)
//! - Subscribes to MarketEvents to keep local topology in sync
//! - Uses an Algorithm to find routes

use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::algorithm::{Algorithm, AlgorithmError};
use crate::events::MarketEvent;
use crate::graph::GraphManager;
use crate::market_data::SharedMarketDataRef;
use crate::types::{OrderSolution, OrderStatus, Solution, SolutionRequest, SolveError};

/// Configuration for a Solver instance.
#[derive(Debug, Clone)]
pub struct SolverConfig {
    /// Name of the algorithm to use.
    pub algorithm_name: String,
    /// Maximum hops to search.
    pub max_hops: usize,
    /// Timeout for solving.
    pub timeout: Duration,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            algorithm_name: "most_liquid".to_string(),
            max_hops: 3,
            timeout: Duration::from_millis(100),
        }
    }
}

/// A solver instance that processes solve requests.
///
/// Each worker thread owns one Solver. The solver initializes the graph on startup
/// from SharedMarketData, and the graph manager maintains the graph and updates it
/// based on market events.
///
/// The solver is generic over the algorithm type `A`, and automatically infers
/// the graph type `G` and graph manager type from the algorithm.
pub struct Solver<A>
where
    A: Algorithm,
    A::GraphType: Send + Sync,
    A::GraphManager: GraphManager<A::GraphType>,
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
    config: SolverConfig,
    /// Whether the graph has been initialized.
    initialized: bool,
}

impl<A> Solver<A>
where
    A: Algorithm,
    A::GraphType: Send + Sync,
    A::GraphManager: GraphManager<A::GraphType>,
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
        config: SolverConfig,
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
    /// Gets the pool topology from SharedMarketData and uses it to build the graph.
    pub async fn initialize_graph(&mut self) {
        let market = self.market_data.read().await;
        let topology = market.pool_topology();
        self.graph_manager.initialize_graph(&topology);
        self.initialized = true;
    }

    /// Processes pending market events.
    ///
    /// Call this periodically or before each solve to stay in sync.
    pub fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_event(&event);
        }
    }

    /// Handles a market event by updating the graph via the graph manager.
    fn handle_event(&mut self, event: &MarketEvent) {
        // Graph manager updates its internal graph based on the event
        self.graph_manager.handle_event(event);
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

        // Solve each order
        let mut order_solutions = Vec::with_capacity(request.orders.len());

        for order in &request.orders {
            // Validate order
            if let Err(_e) = order.validate() {
                order_solutions.push(OrderSolution {
                    order_id: order.id.clone(),
                    status: OrderStatus::NoRouteFound,
                    route: None,
                    amount_in: order.amount_in.unwrap_or_default(),
                    amount_out: alloy::primitives::U256::ZERO,
                    gas_estimate: alloy::primitives::U256::ZERO,
                    price_impact_bps: None,
                    algorithm: String::new(),
                });
                continue;
            }

            // Get the graph from the graph manager
            let graph = self.graph_manager.graph();

            // Find route using algorithm
            let result = self.algorithm.find_best_route(graph, &market, order);

            let order_solution = match result {
                Ok(route) => {
                    let gas_estimate = route.total_gas();
                    let amount_in = route.swaps.first().map(|s| s.amount_in).unwrap_or_default();
                    let amount_out = route.swaps.last().map(|s| s.amount_out).unwrap_or_default();

                    OrderSolution {
                        order_id: order.id.clone(),
                        status: OrderStatus::Success,
                        route: Some(route),
                        amount_in,
                        amount_out,
                        gas_estimate,
                        price_impact_bps: None, // TODO: Calculate price impact
                        algorithm: self.algorithm.name().to_string(),
                    }
                }
                Err(AlgorithmError::NoPath { .. }) => OrderSolution {
                    order_id: order.id.clone(),
                    status: OrderStatus::NoRouteFound,
                    route: None,
                    amount_in: order.amount_in.unwrap_or_default(),
                    amount_out: alloy::primitives::U256::ZERO,
                    gas_estimate: alloy::primitives::U256::ZERO,
                    price_impact_bps: None,
                    algorithm: self.algorithm.name().to_string(),
                },
                Err(AlgorithmError::InsufficientLiquidity) => OrderSolution {
                    order_id: order.id.clone(),
                    status: OrderStatus::InsufficientLiquidity,
                    route: None,
                    amount_in: order.amount_in.unwrap_or_default(),
                    amount_out: alloy::primitives::U256::ZERO,
                    gas_estimate: alloy::primitives::U256::ZERO,
                    price_impact_bps: None,
                    algorithm: self.algorithm.name().to_string(),
                },
                Err(AlgorithmError::Timeout { .. }) => OrderSolution {
                    order_id: order.id.clone(),
                    status: OrderStatus::Timeout,
                    route: None,
                    amount_in: order.amount_in.unwrap_or_default(),
                    amount_out: alloy::primitives::U256::ZERO,
                    gas_estimate: alloy::primitives::U256::ZERO,
                    price_impact_bps: None,
                    algorithm: self.algorithm.name().to_string(),
                },
                Err(_) => OrderSolution {
                    order_id: order.id.clone(),
                    status: OrderStatus::NoRouteFound,
                    route: None,
                    amount_in: order.amount_in.unwrap_or_default(),
                    amount_out: alloy::primitives::U256::ZERO,
                    gas_estimate: alloy::primitives::U256::ZERO,
                    price_impact_bps: None,
                    algorithm: self.algorithm.name().to_string(),
                },
            };

            order_solutions.push(order_solution);
        }

        // Calculate totals
        let total_gas_estimate = order_solutions
            .iter()
            .map(|o| o.gas_estimate)
            .fold(alloy::primitives::U256::ZERO, |acc, g| acc + g);

        let solve_time_ms = start_time.elapsed().as_millis() as u64;

        Ok(Solution {
            orders: order_solutions,
            total_gas_estimate,
            solve_time_ms,
        })
    }

    /// Returns the algorithm name.
    pub fn algorithm_name(&self) -> &str {
        self.algorithm.name()
    }

    /// Returns the config.
    pub fn config(&self) -> &SolverConfig {
        &self.config
    }
}
