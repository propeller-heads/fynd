use crate::models::Route;
use crate::modules::market_graph::MarketGraph;
use num_bigint::BigUint;
use tycho_simulation::tycho_core::Bytes;

// Trait for a generic solving algorithm
pub trait Algorithm {
    fn get_route(&self, token_in: Bytes, token_out: Bytes, amount_in: BigUint) -> Route;
    // Sometimes this returns exactly 1 route (solver and TQ) and sometimes it returns multiple routes (MM)
}

// One algorithm per use case :|

pub struct SimpleAlgorithm {
    max_search_time: u64,
    graph: MarketGraph,
}

impl SimpleAlgorithm {
    pub fn new(max_search_time: u64, graph: MarketGraph) -> Self {
        SimpleAlgorithm {
            max_search_time,
            graph,
        }
    }
}

impl Algorithm for SimpleAlgorithm {
    fn get_route(&self, token_in: Bytes, token_out: Bytes, amount_in: BigUint) -> Route {
        println!("Getting best route from MarketGraph using SimpleAlgorithm between a token pair");
        // 1. get routes between ETH and buy token
        //   a. get_amount_out of all routes
        //   b. choose the route with the highest get_amount_out and use that as buy token price
        // 2. get routes between two tokens
        // 3. simulate get_amount_out on the routes
        // 4. choose the route with the highest get_amount_out
        // Note: it needs to take max_search_time into account
        todo!()
    }
}
