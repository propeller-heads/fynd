//! Solver component that processes solve requests.
//!
//! Each worker thread owns a Solver instance. The solver:
//! - Owns a local copy of the RouteGraph (can be pruned/optimized)
//! - Holds a reference to SharedMarketData (for state lookups)
//! - Subscribes to MarketEvents to keep local graph in sync
//! - Uses an Algorithm to find routes

use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::algorithm::{Algorithm, AlgorithmError};
use crate::events::{MarketEvent, MarketEventHandler};
use crate::market_data::SharedMarketDataRef;
use crate::route_graph::RouteGraph;
use crate::types::{Solution, SolveError, SolutionRequest, OrderSolution, OrderStatus};

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
/// Each worker thread owns one Solver. The solver maintains its own
/// copy of the RouteGraph which it keeps synchronized with market
/// events from the indexer.
pub struct Solver {
    /// Local copy of the route graph (can be pruned/optimized).
    local_graph: RouteGraph,
    /// Algorithm used for route finding.
    algorithm: Box<dyn Algorithm>,
    /// Reference to shared market data.
    market_data: SharedMarketDataRef,
    /// Receiver for market events.
    event_rx: broadcast::Receiver<MarketEvent>,
    /// Configuration.
    config: SolverConfig,
    /// Whether we've received the initial snapshot.
    initialized: bool,
}

impl Solver {
    /// Creates a new Solver.
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
        algorithm: Box<dyn Algorithm>,
        config: SolverConfig,
    ) -> Self {
        Self {
            local_graph: RouteGraph::new(),
            algorithm,
            market_data,
            event_rx,
            config,
            initialized: false,
        }
    }

    /// Synchronizes the local graph from SharedMarketData.
    ///
    /// Call this on startup or when recovering from missed events.
    pub async fn sync_graph(&mut self) {
        let market = self.market_data.read().await;
        self.local_graph = market.clone_route_graph();
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

    /// Solves a request and returns the solution.
    ///
    /// This is the main entry point called by worker threads.
    pub async fn solve(&mut self, request: &SolutionRequest) -> Result<Solution, SolveError> {
        let start_time = Instant::now();

        // Process any pending events first
        self.process_events();

        // Ensure we're initialized
        if !self.initialized {
            self.sync_graph().await;
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

            // Find route using algorithm
            let result = self
                .algorithm
                .find_best_route(&self.local_graph, &market, order);

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

impl MarketEventHandler for Solver {
    fn handle_event(&mut self, event: &MarketEvent) {
        match event {
            MarketEvent::PoolAdded {
                pool_id,
                tokens,
                protocol_system,
            } => {
                self.local_graph
                    .add_pool(pool_id.clone(), tokens, *protocol_system);
            }
            MarketEvent::PoolRemoved { pool_id } => {
                self.local_graph.remove_pool(pool_id);
            }
            MarketEvent::StateUpdated { .. } => {
                // State updates don't affect the graph topology,
                // only the simulation results which are read from SharedMarketData
            }
            MarketEvent::GasPriceUpdated { .. } => {
                // Gas price is read from SharedMarketData during solving
            }
            MarketEvent::Snapshot { pools, .. } => {
                // Full rebuild
                self.local_graph = RouteGraph::new();
                for pool in pools {
                    self.local_graph
                        .add_pool(pool.id.clone(), &pool.tokens, pool.protocol_system);
                }
                self.initialized = true;
            }
        }
    }
}
