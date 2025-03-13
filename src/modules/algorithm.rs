use crate::models::Route;
use num_bigint::BigUint;

pub struct SimpleAlgorithm {
    max_search_time: u64,
}

impl SimpleAlgorithm {
    pub fn new(max_search_time: u64) -> Self {
        SimpleAlgorithm { max_search_time }
    }

    fn get_route(
        &self,
        routes: Vec<Route>,
        amount_in: BigUint,
        gas_price: Option<BigUint>,
    ) -> Route {
        println!("Getting best route from MarketGraph using SimpleAlgorithm between a token pair");
        // 1. get routes between two tokens
        // 2. sort routes by highest spot price (in our previous MostLiquid algo we were also multiplying the spot price by the pools' inertia)
        //    only do this if the algorithm is not fast enough to simulate all routes
        // 3. simulate get_amount_out on the routes on the top n routes
        // 4. if gas_price, is provided deduct the gas_cost from the amount_out
        // 5. choose the route with the highest get_amount_out
        // Note: it needs to take max_search_time into account
        todo!()
    }
}
