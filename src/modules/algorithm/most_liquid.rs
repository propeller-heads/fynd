use num_bigint::BigUint;
use tycho_simulation::{
    tycho_common::{models::protocol::ProtocolComponent, simulation::protocol_sim::ProtocolSim},
    tycho_core::Bytes,
};

use crate::{
    models::{GasPrice, Order, Route},
    modules::{algorithm::algorithm::Algorithm, market_graph::MarketGraph},
};

/// Algorithm that selects routes based on liquidity metrics
pub struct MostLiquidAlgorithm {
    max_search_time: u64,
    /// The market graph data structure owned by this algorithm
    market_graph: MarketGraph,
}

impl Algorithm for MostLiquidAlgorithm {
    fn new(max_hops: usize) -> Self {
        Self {
            max_search_time: 5000, // 5 second default timeout
            market_graph: MarketGraph::new(max_hops),
        }
    }

    fn get_best_route(
        &self,
        order: &Order,
        _gas_price: Option<&GasPrice>,
        _token_out_price: Option<BigUint>,
    ) -> Option<Route> {
        println!("Getting best route using MostLiquidAlgorithm for order {}", order.external_id());

        // Extract tokens and determine routing logic based on exact_out
        let token_in = order.token_in();
        let token_out = order.token_out();

        // Handle exact_in vs exact_out logic
        let routing_amount = if order.exact_out() {
            // For exact_out, we need to reverse-route from the desired output amount
            // For now, use the output amount as an estimate for input calculation
            order
                .amount_out()
                .as_ref()
                .cloned()
                .unwrap_or_default()
        } else {
            // For exact_in, use the input amount directly
            order
                .amount_in()
                .as_ref()
                .cloned()
                .unwrap_or_default()
        };

        println!(
            "Routing from {} to {} with amount {} (exact_out: {})",
            token_in.symbol,
            token_out.symbol,
            routing_amount,
            order.exact_out()
        );

        // 1. Get all possible routes between two tokens from our owned market graph
        // TODO: add extra case for wrapping or unwrapping.
        // If token in is ETH, assume we can start from WETH as well
        // If the token out is ETH, assume we can finish in WETH as well
        // Encoding + execution will take care of wrapping and unwrapping accordingly
        let routes = self
            .market_graph
            .get_routes_between_two_tokens(token_in.address.clone(), token_out.address.clone());

        if routes.is_empty() {
            return None;
        }

        // 2. For each route, calculate expected output and rank by liquidity
        // TODO: Implement actual liquidity-based ranking:
        // - Handle exact_out vs exact_in routing differently
        // - For exact_out: reverse-calculate required input amounts
        // - For exact_in: forward-calculate expected outputs
        // - Sort by spot prices for each path
        // - Simulate get_amount_out/get_amount_in for top N routes
        // - If gas_price and token_out_price are provided, calculate real amount out:
        //   * Estimate gas usage for the route
        //   * Convert gas cost to native token: gas_cost = gas_price * gas_usage
        //   * Deduct gas cost from token_out amount: real_amount_out = amount_out - (gas_cost /
        //     token_out_price_in_eth)
        // - Check that result meets order.min_amount() constraint
        // - Return the route with highest net output (exact_in) or lowest input (exact_out)
        // - Respect max_search_time limit

        // For now, return the first available route
        routes.into_iter().next()
    }

    fn add_market_data(
        &mut self,
        state_id: Bytes,
        component: ProtocolComponent,
        state: Box<dyn ProtocolSim>,
    ) {
        self.market_graph
            .insert(state_id.clone(), component.clone(), state.clone());
    }

    fn remove_market_data(&mut self, state_id: Bytes, component: ProtocolComponent) {
        self.market_graph
            .delete(state_id.clone(), component);
    }

    fn update_market_state(&mut self, state_id: Bytes, new_state: Box<dyn ProtocolSim>) {
        self.market_graph
            .update(state_id.clone(), new_state.clone());
    }
}
