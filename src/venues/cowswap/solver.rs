use crate::modules::executor::Executor;
use crate::venues::cowswap::gateway::CowswapGateway;

struct CowswapSolver {
    gateway: CowswapGateway,
    executor: Executor,
}

impl CowswapSolver {
    fn solve() {
        // - get orders
        // - solve order (single order)
        // - compute clearing prices
        // - encode and submit
        todo!()
    }

    fn compute_clearing_prices() {
        todo!()
    }

    // TODO: I think there are more cowswap specific things like computing the score and etc
}
