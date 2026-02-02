use std::collections::HashMap;

use lazy_static::lazy_static;
use tycho_simulation::{tycho_common::models::Chain, tycho_core::models::Address};

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
        map
    };

    /// Wrapped native token addresses for each chain.
    ///
    /// These are the ERC-20 wrapped versions of each chain's native gas token
    /// (e.g., WETH on Ethereum, WBNB on BSC).
    pub static ref NATIVE_TOKEN: HashMap<Chain, Address> = {
        let mut map = HashMap::new();

        // Ethereum Mainnet - WETH (0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2)
        map.insert(Chain::Ethereum, Address::from([
            0xC0, 0x2a, 0xaA, 0x39, 0xb2, 0x23, 0xFE, 0x8D, 0x0A, 0x0e,
            0x5C, 0x4F, 0x27, 0xeA, 0xD9, 0x08, 0x3C, 0x75, 0x6C, 0xc2,
        ]));

        // Base - WETH (0x4200000000000000000000000000000000000006)
        map.insert(Chain::Base, Address::from([
            0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
        ]));

        // Unichain - WETH (0x4200000000000000000000000000000000000006)
        map.insert(Chain::Unichain, Address::from([
            0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
        ]));

        map
    };
}

/// Error returned when a chain is not supported.
#[derive(Debug, Clone, thiserror::Error)]
#[error("native token not configured for chain: {chain:?}")]
pub struct UnsupportedChainError {
    pub chain: Chain,
}

/// Returns the wrapped native token address for the given chain.
///
/// # Errors
///
/// Returns `UnsupportedChainError` if the chain is not in the registry.
pub fn native_token(chain: &Chain) -> Result<Address, UnsupportedChainError> {
    NATIVE_TOKEN
        .get(chain)
        .cloned()
        .ok_or_else(|| UnsupportedChainError { chain: chain.clone() })
}
