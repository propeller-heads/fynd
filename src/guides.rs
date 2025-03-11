use crate::modules::algorithm::{Algorithm, SimpleAlgorithm};
use crate::modules::executor::Executor;
use crate::modules::market_graph::MarketGraph;
use crate::worker::TychoWorker;
use num_bigint::BigUint;

struct TokenQuoter {
    quote_token: String,
    quote_amount: BigUint,
    worker: TychoWorker,
}

impl TokenQuoter {
    pub fn new(
        quote_token: String,
        quote_amount: BigUint,
        protocols: Vec<String>,
        tokens: Option<String>,
        tvl_filter: f64,
        max_hops: usize,
        max_search_time: u64,
    ) -> Self {
        let graph = MarketGraph::new(max_hops);
        let worker = TychoWorker::new(protocols, tokens, tvl_filter, graph);
        TokenQuoter {
            quote_token,
            quote_amount,
            worker,
        }
    }
    pub fn quote() {
        println!("Quoting the token price");
        // 1. loop through all tokens
        //   a. let routes = self.worker.get_routes(quote_token, token)
        //   b. calculate the mid price and spread
        //   c. choose route with smallest spread
        // 2. return token prices
    }
}

struct Solver {
    worker: TychoWorker,
    algorithm: Box<dyn Algorithm>,
    executor: Executor,
}

impl Solver {
    pub fn new(
        protocols: Vec<String>,
        tokens: Option<String>,
        tvl_filter: f64,
        max_hops: usize,
        max_search_time: u64,
    ) -> Self {
        let graph = MarketGraph::new(max_hops);
        let algorithm = Box::new(SimpleAlgorithm::new(max_search_time, graph.clone()));
        let worker = TychoWorker::new(protocols, tokens, tvl_filter, graph);
        let executor = Executor {};
        Solver {
            worker,
            executor,
            algorithm,
        }
    }
    pub fn solve(token_in: String, token_out: String, amount_in: BigUint) {
        println!("Solving the trade");
        // 1. get routes self.worker.get_routes(token_in, token_out)
        // 2. use self.algorithm to choose the best route
        // 3. encode/execute the trade self.executor(solution)
    }
}

struct MarketMaker {
    worker: TychoWorker,
    executor: Executor,
    token_pair: (String, String),
    target_price: BigUint,
}

impl MarketMaker {
    pub fn new(
        protocols: Vec<String>,
        tokens: Option<String>,
        tvl_filter: f64,
        max_search_time: u64,
        token_pair: (String, String),
        target_price: BigUint,
    ) -> Self {
        let graph = MarketGraph::new(1);
        let worker = TychoWorker::new(protocols, tokens, tvl_filter, graph);
        let executor = Executor {};
        MarketMaker {
            worker,
            executor,
            token_pair,
            target_price,
        }
    }
    pub fn stabilize_market() {
        // infinite loop
        println!("Findings pools that are out of balance and stabilizing them");
        // 1. get routes self.worker.get_route(tokens.0, tokens.1)
        // 2. loop per route:
        //   a. get the mid price and spread
        //   b. if the price is out -> route is unbalanced. => Compute the necessary swap to move the price back TODO: how can we do this*
        // 3. encode/execute the trades self.executor(solution)
    }
}

// TODO: who needs to know about gas?
