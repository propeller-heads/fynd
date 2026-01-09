use std::collections::HashMap;

use lazy_static::lazy_static;

use crate::ProtocolSystem;

lazy_static! {
    /// Average gas cost per swap on each protocol systems.
    pub static ref GAS_COST_PER_SWAP: HashMap<ProtocolSystem, u64> = {
        let mut map = HashMap::new();
        map.insert(ProtocolSystem::UniswapV2, 100_000);
        map.insert(ProtocolSystem::UniswapV3, 150_000);
        map.insert(ProtocolSystem::SushiSwap, 100_000);
        map.insert(ProtocolSystem::Curve, 200_000);
        map.insert(ProtocolSystem::Balancer, 150_000);
        map.insert(ProtocolSystem::Other, 150_000);
        map
    };
}
