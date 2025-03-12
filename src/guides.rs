use crate::modules::algorithm::{Algorithm, SimpleAlgorithm};
use crate::modules::executor::Executor;
use crate::modules::market_graph::MarketGraph;
use crate::solver::TychoSolver;
use num_bigint::BigUint;
use petgraph::Graph;

struct TokenQuoter {
    quote_token: String,
    quote_amount: BigUint,
    solver: TychoSolver,
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
        let algorithm = Box::new(SimpleAlgorithm::new(max_search_time));
        let solver = TychoSolver::new(protocols, tokens, tvl_filter, graph, algorithm);
        TokenQuoter {
            quote_token,
            quote_amount,
            solver,
        }
    }
    pub fn quote() {
        println!("Quoting the token price");
        // 1. loop through all tokens
        //   a. let route = self.solver.get_routes(quote_token, token, quote_amount)
        //   let amount_out = route.get_amount_out(quote_amount) TODO: be better. from the algo we should return a get amount out result or something
        //   b. let inverse_route =  self.solver.get_routes(token, quote_token, amount_out)
        //   c. calculate the mid price and spread
        // 2. return token prices
    }
}

struct Solver {
    solver: TychoSolver,
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
        let algorithm = Box::new(SimpleAlgorithm::new(max_search_time));
        let solver = TychoSolver::new(protocols, tokens, tvl_filter, graph, algorithm);
        let executor = Executor {};
        Solver { solver, executor }
    }
    pub fn solve(token_in: String, token_out: String, amount_in: BigUint) {
        println!("Solving the trade");
        // 1. get routes self.solver.get_route(token_in, token_out, amount_in)
        // 2. encode/execute the trade self.executor(solution)
    }
}

struct MarketMaker {
    solver: TychoSolver,
    executor: Executor,
    token_pair: (String, String),
    target_price: BigUint,
    graph: MarketGraph,
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
        let algorithm = Box::new(SimpleAlgorithm::new(max_search_time));
        let solver = TychoSolver::new(protocols, tokens, tvl_filter, graph.clone(), algorithm);
        let executor = Executor {};
        MarketMaker {
            solver,
            executor,
            token_pair,
            target_price,
            graph,
        }
    }
    pub fn stabilize_market() {
        // infinite loop
        println!("Findings pools that are out of balance and stabilizing them");
        // 1. get routes self.graph.get_routes(tokens.0, tokens.1)
        // 2. loop per route:
        //   a. get the mid price and spread
        //   b. if the price is out -> route is unbalanced.
        //   c. compute the necessary swap to move the price back:
        //     i. run simulations iteratively to find the necessary swap to move the price back
        //     ii. if the price of the swap (accounting for gas) is higher than the target_price, don't do this trade
        // 3. encode/execute the trades self.executor(solution)
    }
}
