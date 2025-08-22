use crate::models::Route;
use num_bigint::BigUint;

pub trait Algorithm {
    fn get_best_route(
        &self,
        routes: Vec<Route>,
        amount_in: BigUint,
        gas_price: Option<BigUint>,
    ) -> Route;

    // TODO: define a nice and general Algorithm interface. It needs to:
    //   - solve for the best route
    //   - handle messages from Tycho Indexer to update the Graph (in this case it is the graph
    //     but there might be other algorithms that don't need a graph so this needs to be general enough)
}
