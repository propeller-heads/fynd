use crate::modules::executor::Executor;
use crate::venues::uniswap_x::gateway::UniswapXGateway;

struct UniswapXSolver {
    gateway: UniswapXGateway,
    executor: Executor,
}

impl UniswapXSolver {
    fn solve() {
        // - get orders
        // - solve order (single order)
        // - encode and execute
        todo!()
    }

    fn compute_clearing_prices() {
        todo!()
    }

    // TODO: I think there are more cowswap specific things like computing the score and etc
}
