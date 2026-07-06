use std::collections::HashMap;

use lazy_static::lazy_static;
use tycho_simulation::{tycho_common::models::Chain, tycho_core::models::Address};

lazy_static! {
    /// Wrapped native token addresses for each chain.
    ///
    /// These are the ERC-20 wrapped versions of each chain's native gas token
    /// (e.g., WETH on Ethereum, WBNB on BSC).
    pub(crate) static ref NATIVE_TOKEN: HashMap<Chain, Address> = {
        let mut map = HashMap::new();

        // Ethereum Mainnet - WETH (0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2)
        map.insert(Chain::Ethereum, Address::from([
            0xC0, 0x2A, 0xAA, 0x39, 0xB2, 0x23, 0xFE, 0x8D, 0x0A, 0x0E,
            0x5C, 0x4F, 0x27, 0xEA, 0xD9, 0x08, 0x3C, 0x75, 0x6C, 0xC2,
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

        // Arbitrum - WETH (0x82aF49447D8a07e3bd95BD0d56f35241523fBab1)
        map.insert(Chain::Arbitrum, Address::from([
            0x82, 0xAF, 0x49, 0x44, 0x7D, 0x8A, 0x07, 0xE3, 0xBD, 0x95,
            0xBD, 0x0D, 0x56, 0xF3, 0x52, 0x41, 0x52, 0x3F, 0xBA, 0xB1,
        ]));

        // Polygon - WPOL/WMATIC (0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270)
        map.insert(Chain::Polygon, Address::from([
            0x0D, 0x50, 0x0B, 0x1D, 0x8E, 0x8E, 0xF3, 0x1E, 0x21, 0xC9,
            0x9D, 0x1D, 0xB9, 0xA6, 0x44, 0x4D, 0x3A, 0xDF, 0x12, 0x70,
        ]));

        // BSC - WBNB (0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c)
        map.insert(Chain::Bsc, Address::from([
            0xBB, 0x4C, 0xDB, 0x9C, 0xBD, 0x36, 0xB0, 0x1B, 0xD1, 0xCB,
            0xAE, 0xBF, 0x2D, 0xE0, 0x8D, 0x91, 0x73, 0xBC, 0x09, 0x5C,
        ]));

        // Flare - WFLR (0x1D80c49BbBCd1C0911346656B529DF9E5c2F783d)
        map.insert(Chain::Flare, Address::from([
            0x1D, 0x80, 0xC4, 0x9B, 0xBB, 0xCD, 0x1C, 0x09, 0x11, 0x34,
            0x66, 0x56, 0xB5, 0x29, 0xDF, 0x9E, 0x5C, 0x2F, 0x78, 0x3D,
        ]));

        map
    };
}

/// Error returned when a chain is not supported.
#[derive(Debug, Clone, thiserror::Error)]
#[error("native token not configured for chain: {chain:?}")]
pub struct UnsupportedChainError {
    pub(crate) chain: Chain,
}

impl UnsupportedChainError {
    /// Returns the unsupported chain.
    pub fn chain(&self) -> Chain {
        self.chain
    }
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
        .ok_or(UnsupportedChainError { chain: *chain })
}

/// Parses a chain name string (case-insensitive) into a [`Chain`] enum.
///
/// Uses serde deserialization so it automatically supports all `Chain` variants.
///
/// # Errors
///
/// Returns an error if the chain name is not recognized.
pub fn parse_chain(chain: &str) -> Result<Chain, ParseChainError> {
    let candidate = format!("\"{}\"", chain.to_ascii_lowercase());
    serde_json::from_str::<Chain>(&candidate).map_err(|_| ParseChainError(chain.to_string()))
}

/// Error returned when a chain name string cannot be parsed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unsupported chain '{0}'")]
pub struct ParseChainError(pub String);
