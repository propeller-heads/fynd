use crate::modules::algorithm::Algorithm;
use crate::modules::market_graph::MarketGraph;

pub struct TychoWorker {
    protocols: Vec<String>,
    tokens: Option<String>,
    tvl_filter: f64,

    graph: MarketGraph,
    algorithm: Box<dyn Algorithm>,
}

impl TychoWorker {
    pub fn new(
        protocols: Vec<String>,
        tokens: Option<String>,
        tvl_filter: f64,
        algorithm: Box<dyn Algorithm>,
        graph: MarketGraph,
    ) -> Self {
        TychoWorker {
            protocols,
            tokens,
            tvl_filter,
            graph,
            algorithm,
        }
    }

    fn run() {
        // 1. spawn a task to connect to tycho indexer with self.protocols
        // 2. spawn a task to get gas price
        // 3. infinite loop to listen to the indexer. Only care about tokens in self.tokens (if defined)
        //   a. If it's the first message -> init the graph
        //   b. If it's a new block -> update the graph
    }

    fn get_route() {
        // let routes = self.graph.get_routes(token_in, token_out)
        // self.algorithm.get_best_route(routes)
        println!("Getting route from TychoWorker");
    }
}
