use std::{collections::HashMap, sync::Arc};

use alloy::primitives::B256;
use num_bigint::BigUint;
use tokio::sync::Mutex;
use tycho_execution::encoding::models::Solution;
use tycho_router::{
    models::{GasPrice, Order, Route},
    modules::{
        algorithm::algorithm::Algorithm,
        execution::{executor::Executor, models::SolutionExt},
    },
    solver::Solver,
};
use tycho_simulation::tycho_common::Bytes;

use crate::{gateway::UniswapXGateway, models::UniswapXError};
use tycho_router::modules::execution::{executor::ExecutorError, models::SolutionExtError};

/// UniswapX solver error types
#[derive(Debug)]
pub enum UniswapXSolverError {
    UniswapX(String),
    TychoRouter(String),
    Config(String),
    Routing(String),
    External(String),
}

impl std::fmt::Display for UniswapXSolverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UniswapX(msg) => write!(f, "UniswapX integration error: {}", msg),
            Self::TychoRouter(msg) => write!(f, "Tycho router error: {}", msg),
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::Routing(msg) => write!(f, "Routing failed: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for UniswapXSolverError {}

impl From<UniswapXError> for UniswapXSolverError {
    fn from(err: UniswapXError) -> Self {
        Self::UniswapX(err.to_string())
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for UniswapXSolverError {
    fn from(err: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::External(err.to_string())
    }
}

impl From<SolutionExtError> for UniswapXSolverError {
    fn from(err: SolutionExtError) -> Self {
        Self::TychoRouter(err.to_string())
    }
}

impl From<ExecutorError> for UniswapXSolverError {
    fn from(err: ExecutorError) -> Self {
        Self::External(err.to_string())
    }
}

/// Configuration for how the UniswapX solver should behave
#[derive(Clone, Debug)]
pub struct UniswapXSolverConfig {
    pub min_profit_threshold: f64, /* Minimum profit in ETH to execute
                                    * TODO: Add deadline strategy configuration
                                    * - Determine when to stop solving and start executing
                                    * - Consider block timing, gas prices, MEV protection
                                    * - Balance between finding better solutions vs execution
                                    *   timing */
}

/// UniswapX solver that orchestrates the complete pipeline:
/// Gateway -> tycho-router Solver -> Executor
#[derive(Clone)]
pub struct UniswapXSolver<A: Algorithm> {
    config: UniswapXSolverConfig,
    /// - UniswapXGateway (handles API calls)
    gateway: Arc<Mutex<UniswapXGateway>>,
    /// - tycho-router Solver (handles routing)
    solver: Arc<Mutex<Solver<A>>>,
    /// - tycho-router Executor (handles transactions)
    executor: Executor,
}

impl<A: Algorithm + Send + 'static> UniswapXSolver<A> {
    pub fn new(
        gateway: UniswapXGateway,
        solver: Solver<A>,
        executor: Executor,
        config: UniswapXSolverConfig,
    ) -> Self {
        Self {
            gateway: Arc::new(Mutex::new(gateway)),
            solver: Arc::new(Mutex::new(solver)),
            executor,
            config,
        }
    }

    /// Main processing function: fetch orders from UniswapX and process them
    /// This is the main entry point that coordinates everything with deadline strategy
    pub async fn process_orders(&self) -> Result<Vec<B256>, UniswapXSolverError> {
        println!("Fetching orders from UniswapX API...");

        // 1. Gateway fetches and converts orders
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let current_block = 18_500_000u64; // TODO: Get actual current block number

        let orders = {
            let mut gateway = self.gateway.lock().await;
            let solver = self.solver.lock().await;
            let gas_price = solver.get_gas_price();
            let tokens = solver.get_tokens();
            gateway
                .get_processable_orders(current_timestamp, current_block, gas_price, tokens)
                .await?
        };
        println!("Found {} fillable orders from UniswapX", orders.len());

        // TODO: Implement deadline strategy
        // - Calculate deadline based on block timing and gas conditions from solver
        // - Process orders with time awareness
        // - Stop solving when deadline approaches and execute best solutions
        // - Use gas_price from solver for deadline calculations

        let mut solutions = Vec::new();

        for order in orders {
            if let Some(route) = self.process_single_order(&order).await? {
                let solution = Solution::from_order_route_pair(&order, &route)?;
                solutions.push(solution);
            }
        }

        let txs = self.executor.encode(&solutions)?;
        // 5. Add Uniswap X specific call data to each tx like in the uniswap x encoding example
        // 6. Simulate transaction
        let sim_result = self
            .executor
            .simulate(&txs)
            .await?;
        // 7. Execute if simulation passes for that order
        let tx_hashes = self.executor.execute(&txs).await?;

        Ok(tx_hashes)
    }

    /// Process a single order through the complete pipeline
    async fn process_single_order(
        &self,
        order: &Order,
    ) -> Result<Option<Route>, Box<dyn std::error::Error + Send + Sync>> {
        let order_id = order.external_id().to_string();

        // 1. Use tycho-router Solver to find best route
        let route = {
            let solver = self.solver.lock().await;
            solver.solve_order(&order)
        };

        let route = match route {
            Some(route) => route,
            None => {
                println!("No route found for order {}", order_id);
                return Ok(None);
            }
        };

        // 2. Get gas price and token prices from solver for profit calculation
        let gas_price_opt = self.get_gas_price().await?;
        let token_prices = self.get_token_prices().await?;

        // Skip if we don't have gas price yet
        let gas_price = match gas_price_opt {
            Some(price) => price,
            None => {
                println!("Order {} skipped - no gas price available yet", order_id);
                return Ok(None);
            }
        };

        // 3. Calculate profit estimate using solver data
        let profit_estimate =
            Self::calculate_profit_estimate(&order, &route, &gas_price, &token_prices);

        // 4. Check minimum profit threshold
        if let Some(profit) = profit_estimate {
            if profit < self.config.min_profit_threshold {
                println!(
                    "Order {} profit {:.4} below threshold {:.4}",
                    order_id, profit, self.config.min_profit_threshold
                );
                return Ok(None);
            }
        }

        Ok(Some(route))
    }

    fn calculate_profit_estimate(
        _order: &Order,
        _route: &Route,
        _gas_price: &GasPrice,
        _token_prices: &HashMap<Bytes, BigUint>,
    ) -> Option<f64> {
        // TODO: Calculate expected profit in USD using gas price and token prices from solver
        // - Get route output amount
        // - Convert to USD using token_prices from solver
        // - Calculate gas costs: estimated_gas * gas_price * ETH_price_in_USD
        // - Subtract any fees (protocol fees, MEV protection costs)
        // - Return net profit in USD
        Some(10.0) // Placeholder
    }

    /// Get current gas price from the inner solver
    async fn get_gas_price(&self) -> Result<Option<GasPrice>, UniswapXError> {
        let solver = self.solver.lock().await;
        Ok(solver.get_gas_price().cloned())
    }

    /// Get current token prices from the inner solver
    async fn get_token_prices(&self) -> Result<HashMap<Bytes, BigUint>, UniswapXError> {
        let solver = self.solver.lock().await;
        Ok(solver.get_token_prices().clone())
    }

    /// Run continuous polling loop to process new orders as they appear
    /// This is the main background service that keeps the order processing pipeline active
    pub async fn run_polling_loop(
        &self,
        poll_interval_secs: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));

        println!("Starting UniswapX polling loop (interval: {}s)", poll_interval_secs);

        loop {
            interval.tick().await;

            match self.process_orders().await {
                Ok(tx_hashes) => {
                    if !tx_hashes.is_empty() {
                        println!("Processed batch: {} transactions executed", tx_hashes.len());
                        for (i, tx_hash) in tx_hashes.iter().enumerate() {
                            println!("  Transaction {}: {}", i + 1, tx_hash);
                        }
                    } else {
                        println!("No profitable orders found in this cycle");
                    }

                    // TODO: Add batch processing metrics:
                    // - Track processing time
                    // - Monitor API rate limits
                    // - Track success/failure rates
                    // - Alert on prolonged failures
                }
                Err(e) => {
                    eprintln!("Error processing orders: {}", e);
                    // TODO: Add error handling strategies:
                    // - Exponential backoff for API rate limits
                    // - Circuit breaker for persistent failures
                    // - Alert mechanisms for critical errors
                }
            }

            // TODO: Add periodic maintenance tasks:
            // - Clean up old order book entries
            // - Update token price cache
            // - Rotate logs
            // - Health check reporting
        }
    }

    /// Start background polling in a separate task
    /// Returns a handle that can be used to monitor or cancel the polling
    pub fn start_background_polling(
        self,
        poll_interval_secs: u64,
    ) -> tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        tokio::spawn(async move {
            self.run_polling_loop(poll_interval_secs)
                .await
        })
    }

    /// Stop processing and mark any in-progress orders as failed
    pub async fn shutdown(&self) -> Result<(), UniswapXSolverError> {
        println!("Shutting down UniswapX solver...");

        // TODO: Implement graceful shutdown:
        // - Mark all processing orders as failed
        // - Save order book state
        // - Close API connections
        // - Wait for in-flight transactions

        let mut gateway = self.gateway.lock().await;
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Mark any processing orders as failed during shutdown
        // (This is a simplified version - in practice we'd iterate through processing orders)
        println!("Marking in-progress orders as failed due to shutdown");

        Ok(())
    }
}
