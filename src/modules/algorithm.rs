use crate::modules::market_graph::MarketGraph;
use num_bigint::BigUint;

pub trait Algorithm {
    fn get_route(&self);
    // Sometimes this returns exactly 1 route (solver and TQ) and sometimes it returns multiple routes (MM)
    fn get_route_amount_out(&self);
    // maybe this could be a method of the Route object as well
    // it should take an optional token price parameter. if it's passed, calculate the gas price in buy token and deduct from the amount_out
}

// One algorithm per use case :|

pub struct SimpleAlgorithm {
    max_search_time: u64,
    gas_price: BigUint,
    graph: MarketGraph,
}

impl SimpleAlgorithm {
    pub fn new(max_search_time: u64, graph: MarketGraph) -> Self {
        SimpleAlgorithm {
            max_search_time,
            gas_price: BigUint::ZERO,
            graph,
        }
    }
}

impl Algorithm for SimpleAlgorithm {
    fn get_route(&self) {
        println!("Getting best route from MarketGraph using SimpleAlgorithm between a token pair");
        // 1. get routes between ETH and buy token
        //   a. get_amount_out of all routes
        //   b. choose the route with the highest get_amount_out and use that as buy token price
        // 2. get routes between two tokens
        // 3. simulate get_amount_out on the routes
        // 4. choose the route with the highest get_amount_out
        // Note: it needs to take max_search_time into account
    }

    fn get_route_amount_out(&self) {
        println!("Getting route amount out considering gas");
    }
}

pub struct MidPriceAlgorithm {
    max_search_time: u64,
    gas_price: BigUint,
    graph: MarketGraph,
}

impl MidPriceAlgorithm {
    pub fn new(max_search_time: u64, graph: MarketGraph) -> Self {
        MidPriceAlgorithm {
            max_search_time,
            gas_price: BigUint::ZERO,
            graph,
        }
    }
}

impl Algorithm for MidPriceAlgorithm {
    fn get_route(&self) {
        println!(
            "Getting best route from MarketGraph using MidPriceAlgorithm between a token pair"
        );
        // 1. get routes between two tokens
        // 2. simulate get_amount_out on the routes and get_amount_out_reverse
        // 3. subtract gas
        // 4. calculate the mid price and spread
        // 5. choose the route with the smalled mid price
    }

    fn get_route_amount_out(&self) {}
}

pub struct BalancePoolAlgorithm {
    max_search_time: u64,
    gas_price: BigUint,
    graph: MarketGraph,
}

impl BalancePoolAlgorithm {
    pub fn new(max_search_time: u64, graph: MarketGraph) -> Self {
        BalancePoolAlgorithm {
            max_search_time,
            gas_price: BigUint::ZERO,
            graph,
        }
    }
}

impl Algorithm for BalancePoolAlgorithm {
    fn get_route(&self) {
        println!(
            "Getting best route from MarketGraph using MidPriceAlgorithm between a token pair"
        );
        // 1. Get routes between the two tokens
        // 2. Calculate spot price of all pools
        // 3. Return the pools where the price is out of balance
        // TODO: is this algo even necessary? it isn't doing anything at this point tbh
    }

    fn get_route_amount_out(&self) {}
}
