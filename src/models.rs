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

    pub fn get_amount_out_inverse_route(&self, amount_in: BigUint) -> BigUint {
        // 1. loop through all pools in reverse order
        // 2. simulate get_amount_out
        // 3. return the amount_out
        todo!()
    }
    pub fn mid_price(&self, gas_price: BigUint) -> (BigUint, BigUint) {
        // 2. simulate get_amount_out and get_amount_out_reverse
        // 3. subtract gas
        // 4. return the mid price and spread
        todo!()
    }
}
