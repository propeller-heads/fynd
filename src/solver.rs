use crate::models::Route;
use crate::modules::algorithms::algorithm::Algorithm;
use num_bigint::BigUint;
use std::collections::HashMap;
use tycho_simulation::tycho_core::Bytes;

pub struct TychoSolver<A> {
    protocols: Vec<String>,
    tokens: Option<String>,
    tvl_filter: f64,
    algorithm: A,
    gas_price: BigUint,
}

impl<A> TychoSolver<A>
where
    A: Algorithm,
{
    pub fn new(
        protocols: Vec<String>,
        tokens: Option<String>,
        tvl_filter: f64,
        algorithm: A,
    ) -> Self {
        TychoSolver {
            protocols,
            tokens,
            tvl_filter,
            algorithm,
            gas_price: BigUint::ZERO,
        }
    }

    fn run() {
        // 1. spawn a task to connect to tycho indexer with self.protocols
        // 2. spawn a task to get gas price and set it in the worker
        // 3. infinite loop to listen to the indexer. Only care about tokens in self.tokens (if defined)
        //   a. If it's the first message -> init the graph
        //   b. If it's a new block -> update the graph
        // TODO: define a way to pass updates to the graph through the Algorithm
    }

    fn get_route(
        &self,
        token_in: Bytes,
        token_out: Bytes,
        amount_in: BigUint,
        token_prices: Option<HashMap<Bytes, BigUint>>,
    ) -> Route {
        // let routes = self.graph.get_routes(token_in, token_out)
        // if token_prices are passed, calculate the gas price in token_out
        // let route = self.algorithm.get_route(routes, amount_in, gas_price)
        println!("Getting best route");
        todo!()
    }
}
