use crate::models::Route;
use crate::modules::market_graph::MarketGraph;
use num_bigint::BigUint;
use tycho_simulation::tycho_core::Bytes;

pub struct TychoWorker {
    protocols: Vec<String>,
    tokens: Option<String>,
    tvl_filter: f64,

    graph: MarketGraph,
    gas_price: BigUint,
}

impl TychoWorker {
    pub fn new(
        protocols: Vec<String>,
        tokens: Option<String>,
        tvl_filter: f64,
        graph: MarketGraph,
    ) -> Self {
        TychoWorker {
            protocols,
            tokens,
            tvl_filter,
            graph,
            gas_price: BigUint::ZERO,
        }
    }

    fn run() {
        // 1. spawn a task to connect to tycho indexer with self.protocols
        // 2. spawn a task to get gas price and set it in the worker
        // 3. infinite loop to listen to the indexer. Only care about tokens in self.tokens (if defined)
        //   a. If it's the first message -> init the graph
        //   b. If it's a new block -> update the graph
    }

    fn get_routes(&self, token_in: Bytes, token_out: Bytes) -> Vec<Route> {
        // let routes = self.graph.get_routes(token_in, token_out)
        println!("Getting route from TychoWorker");
        todo!()
    }
}
