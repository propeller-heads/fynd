use num_bigint::BigUint;
use tycho_simulation::models::Token;
use tycho_simulation::protocol::state::ProtocolSim;

pub struct Route {
    tokens: Vec<Token>,
    pools: Vec<Box<dyn ProtocolSim>>,
}

impl Route {
    pub fn get_amount_out(&self, amount_in: BigUint) -> BigUint {
        // 1. loop through all pools
        // 2. simulate get_amount_out
        // 3. return the amount_out
        todo!()
    }

    pub fn spot_price(&self) -> BigUint {
        // loop through all pools and corresponding tokens
        // multiply the spot prices and subtract the pool's fee
        todo!()
    }
}

// TODO: define order
