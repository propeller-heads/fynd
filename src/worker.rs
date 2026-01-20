//! A Solver Worker that processes solve requests.
//!
//! The Solver Worker:
//! - Holds a reference to SharedMarketData (for state lookups)
//! - Subscribes to MarketEvents to update the algorithm's internal state
//! - Uses a stateful Algorithm that owns its internal data structures (graph, etc.)

use std::time::{Duration, Instant};

use num_bigint::BigUint;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{
    algorithm::{Algorithm, AlgorithmError},
    feed::{events::MarketEvent, market_data::SharedMarketDataRef},
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
/// The worker initializes the algorithm on startup from SharedMarketData.
/// The algorithm maintains its own internal state (graph, indices, etc.) and
/// updates it based on market events.
pub struct SolverWorker<A>
where
    A: Algorithm,
{
    /// Algorithm used for route finding (owns its internal state).
    algorithm: A,
    /// Reference to shared market data.
    market_data: SharedMarketDataRef,
    /// Receiver for market events.
    event_rx: broadcast::Receiver<MarketEvent>,
    /// Configuration.
    config: WorkerConfig,
    /// Whether the algorithm has been initialized.
    initialized: bool,
}

impl<A> SolverWorker<A>
where
    A: Algorithm,
{
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
        algorithm: A,
        config: WorkerConfig,
    ) -> Self {
        Self { algorithm, market_data, event_rx, config, initialized: false }
    }

    /// Initializes the algorithm from SharedMarketData.
    ///
    /// Call this on startup or when recovering from missed events.
    /// The algorithm acquires locks internally.
    pub async fn initialize(&mut self) {
        self.algorithm
            .initialize(self.market_data.clone())
            .await;
        self.initialized = true;
    }

    /// Drains pending events from the channel.
    fn drain_events(&mut self) -> Vec<MarketEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Solves a request and returns the solution.
    ///
    /// This is the main entry point called by worker threads.
    pub async fn solve(&mut self, request: &SolutionRequest) -> Result<Solution, SolveError> {
        let start_time = Instant::now();

        // Drain events first (no locks needed)
        let pending_events = self.drain_events();

        // Ensure we're initialized FIRST (before handling events)
        // Events are incremental updates that only make sense on an initialized graph
        if !self.initialized {
            self.algorithm
                .initialize(self.market_data.clone())
                .await;
            self.initialized = true;
        }

        // Process pending events as a batch (algorithm handles locking internally)
        if !pending_events.is_empty() {
            if let Err(e) = self
                .algorithm
                .handle_events(&pending_events, self.market_data.clone())
                .await
            {
                warn!("Warning: Error handling market events: {:?}", e);
            }
        }

        // Get block info for the solution
        // TODO: Make BlockInfo access atomic so we don't need to acquire a lock here
        let block_info = {
            let market = self.market_data.read().await;
            market
                .last_updated()
                .cloned()
                .ok_or(SolveError::Internal(
                    "No block info available, this means no block has been processed yet"
                        .to_string(),
                ))?
        };

        // Solve each order
        let mut order_solutions = Vec::with_capacity(request.orders.len());

        for order in &request.orders {
            // Log order details once at entry
            debug!(
                order_id = %order.id,
                token_in = ?order.token_in,
                token_out = ?order.token_out,
                amount = %order.amount,
                side = ?order.side,
                "processing order"
            );

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

            // Find route using algorithm (algorithm handles locking internally)
            let result = self
                .algorithm
                .find_best_route(self.market_data.clone(), order)
                .await;

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
