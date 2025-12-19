use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use tycho_execution::encoding::models::{Solution, Transaction};

use crate::{
    models::{Order, Route, SolverError},
    modules::{
        algorithm::algorithm::Algorithm,
        execution::{
            executor::{Executor, ExecutorError, TransactionStatus},
            models::{SolutionExt, SolutionExtError},
        },
    },
    solver::Solver,
};

/// Response from solve endpoint (routes + encoded transactions, no execution)
#[derive(Debug, Serialize, Deserialize)]
pub struct SolveResponse {
    pub routes: Vec<Route>,
    pub transactions: Vec<Transaction>,
}

/// Response from solve_and_execute endpoint (routes + transactions + execution)
#[derive(Debug, Serialize, Deserialize)]
pub struct SolveAndExecuteResponse {
    pub routes: Vec<Route>,
    pub transactions: Vec<Transaction>,
    /// Map of order ID to transaction hash after execution
    pub transaction_hashes: HashMap<String, String>,
}

/// Response for tracking endpoint
#[derive(Debug, Serialize, Deserialize)]
pub struct TrackTransactionResponse {
    pub transaction_statuses: HashMap<String, TransactionStatus>,
}

pub struct RouterApi<A: Algorithm> {
    solver: Solver<A>,
    executor: Executor,
}

impl<A: Algorithm + Send + 'static> RouterApi<A> {
    pub fn new(solver: Solver<A>, executor: Executor) -> Self {
        Self { solver, executor }
    }

    /// Solve orders and return routes with encoded transactions (no execution)
    ///
    /// This finds the best routes for each order, converts them to solutions,
    /// encodes them into executable transactions, and returns everything
    /// without executing on-chain.
    pub async fn solve(&self, orders: &[Order]) -> Result<SolveResponse, SolverError> {
        // Find routes for all orders
        let mut routes = Vec::new();
        for order in orders {
            let route = self
                .solver
                .solve_order(order)
                .await
                .ok_or(SolverError::Algorithm(format!(
                    "No route found for order {}",
                    order.external_id()
                )))?;
            routes.push(route);
        }

        // Convert routes to solutions
        let mut solutions = Vec::new();
        for (order, route) in orders.iter().zip(routes.iter()) {
            let solution = Solution::from_order_route_pair(order, route)
                .map_err(|e| SolverError::Execution(e.to_string()))?;
            solutions.push(solution);
        }

        // Encode solutions to get executable transactions
        let transactions = self
            .executor
            .encode(&solutions)
            .map_err(|e| SolverError::Execution(e.to_string()))?;

        Ok(SolveResponse { routes, transactions })
    }

    /// Solve orders, encode transactions, and execute them immediately
    ///
    /// This performs the same solving and encoding as solve(), but then
    /// proceeds to execute the transactions on-chain and returns the
    /// transaction hashes along with the routes and transactions.
    pub async fn solve_and_execute(
        &mut self,
        orders: &[Order],
    ) -> Result<SolveAndExecuteResponse, SolverError> {
        // Find routes and create solutions (same as solve, but we need raw transactions for
        // execution)
        let mut routes = Vec::new();
        for order in orders {
            let route = self
                .solver
                .solve_order(order)
                .await
                .ok_or(SolverError::Algorithm(format!(
                    "No route found for order {}",
                    order.external_id()
                )))?;
            routes.push(route);
        }

        // Convert routes to solutions
        let mut solutions = Vec::new();
        for (order, route) in orders.iter().zip(routes.iter()) {
            let solution = Solution::from_order_route_pair(order, route)
                .map_err(|e| SolverError::Execution(e.to_string()))?;
            solutions.push(solution);
        }

        // Encode solutions to get executable transactions
        let transactions = self
            .executor
            .encode(&solutions)
            .map_err(|e| SolverError::Execution(e.to_string()))?;

        // Execute all transactions on-chain
        let tx_hashes = self
            .executor
            .execute(&transactions)
            .await
            .map_err(|e| SolverError::Execution(e.to_string()))?;

        // Create HashMap mapping order IDs to transaction hashes
        let mut transaction_hashes = HashMap::new();
        for (order, hash) in orders.iter().zip(tx_hashes.iter()) {
            transaction_hashes.insert(
                order.external_id().to_string(),
                format!("0x{}", hex::encode(hash.as_slice())),
            );
        }

        Ok(SolveAndExecuteResponse {
            routes,
            transactions,
            transaction_hashes,
        })
    }

    /// Track the status of transactions by their hashes
    ///
    /// This takes a list of transaction hashes and returns their current status
    /// including confirmation count, block number, and execution status.
    pub async fn track_transactions(
        &self,
        tx_hashes: &[String],
    ) -> Result<TrackTransactionResponse, SolverError> {
        let transaction_statuses = self
            .executor
            .track_transactions(tx_hashes)
            .await
            .map_err(|e| SolverError::Execution(e.to_string()))?;

        Ok(TrackTransactionResponse {
            transaction_statuses,
        })
    }

    /// Get a reference to the underlying solver
    pub fn solver(&self) -> &Solver<A> {
        &self.solver
    }

    /// Get a mutable reference to the underlying solver
    pub fn solver_mut(&mut self) -> &mut Solver<A> {
        &mut self.solver
    }

    /// Get a reference to the underlying executor
    pub fn executor(&self) -> &Executor {
        &self.executor
    }
}

// Error type conversions
impl From<SolutionExtError> for SolverError {
    fn from(err: SolutionExtError) -> Self {
        SolverError::Execution(err.to_string())
    }
}

impl From<ExecutorError> for SolverError {
    fn from(err: ExecutorError) -> Self {
        SolverError::Execution(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // TODO: Add unit tests for RouterApi
    // - Test quote() returns routes for multiple orders without executing
    // - Test solve() returns routes and executes transactions for multiple orders
    // - Test error handling for invalid orders (partial failures)
    // - Test batch optimization (all orders in single transaction vs multiple)
    // - Mock the solver and executor for isolated testing
}
