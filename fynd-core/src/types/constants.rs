use std::str::FromStr;

use num_bigint::BigUint;
use tycho_simulation::{
    tycho_common::models::Chain,
    tycho_core::{models::Address, simulation::protocol_sim::Price},
};

/// The routable representation used to price native gas and the conversion between its raw units
/// and the chain's native gas units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GasTokenConfig {
    pub(crate) address: Address,
    pub(crate) probe_amount: BigUint,
    /// Routable-token raw units with the same economic value as one native gas raw unit.
    pub(crate) native_to_routable_unit: Price,
}

impl Default for GasTokenConfig {
    fn default() -> Self {
        Self {
            address: Address::zero(20),
            probe_amount: BigUint::from(10u8).pow(18),
            native_to_routable_unit: Price {
                numerator: BigUint::from(1u8),
                denominator: BigUint::from(1u8),
            },
        }
    }
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

/// Resolves the routable representation used to price native gas.
///
/// # Errors
/// Returns `UnsupportedChainError` if the chain is not registered or has no routable native-asset
/// representation. Wrapper chains use their wrapper token. Shared-balance chains such as Arc use
/// their real routable representation without inventing a wrapper.
pub(crate) fn gas_token_config(chain: &Chain) -> Result<GasTokenConfig, UnsupportedChainError> {
    let unsupported = || UnsupportedChainError { chain: *chain };
    let native_asset = chain
        .try_native_asset()
        .map_err(|_| unsupported())?;
    let native = native_asset.native_token();
    let routable = native_asset
        .routable_token()
        .ok_or_else(unsupported)?;

    Ok(GasTokenConfig {
        address: routable.address.clone(),
        probe_amount: BigUint::from(10u8).pow(routable.decimals),
        native_to_routable_unit: Price {
            numerator: BigUint::from(10u8).pow(routable.decimals),
            denominator: BigUint::from(10u8).pow(native.decimals),
        },
    })
}

/// Returns the routable native-asset representation used by the solver for gas pricing.
///
/// # Errors
/// Returns `UnsupportedChainError` under the same conditions as `gas_token_config`.
pub fn native_token(chain: &Chain) -> Result<Address, UnsupportedChainError> {
    gas_token_config(chain).map(|config| config.address)
}

/// Parses a chain name string (case-insensitive) into a [`Chain`].
///
/// Built-in chains resolve directly; any other name resolves against the custom-chain registry
/// (populated from `TYCHO_CHAINS_CONFIG`), so self-hosted custom chains work once the registry is
/// installed. Returns an error if the name is neither built-in nor a registered custom chain.
pub fn parse_chain(chain: &str) -> Result<Chain, ParseChainError> {
    Chain::from_str(&chain.to_ascii_lowercase()).map_err(|_| ParseChainError(chain.to_string()))
}

/// Error returned when a chain name string cannot be parsed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unsupported chain '{0}'")]
pub struct ParseChainError(pub String);

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::tycho_common::Bytes;

    use super::*;

    #[test]
    fn arc_uses_routable_usdc_for_gas_pricing() {
        let config = gas_token_config(&Chain::Arc).unwrap();

        assert_eq!(
            config.address,
            Bytes::from_str("0x3600000000000000000000000000000000000000").unwrap()
        );
        assert_eq!(config.probe_amount, BigUint::from(1_000_000u64));
        assert_eq!(config.native_to_routable_unit.numerator, BigUint::from(1_000_000u64));
        assert_eq!(
            config
                .native_to_routable_unit
                .denominator,
            BigUint::from(10u8).pow(18)
        );
        assert_eq!(native_token(&Chain::Arc).unwrap(), config.address);
    }

    #[test]
    fn wrapper_chains_keep_equal_native_and_routable_units() {
        let config = gas_token_config(&Chain::Ethereum).unwrap();

        assert_eq!(config.probe_amount, BigUint::from(10u8).pow(18));
        assert_eq!(config.native_to_routable_unit.numerator, BigUint::from(10u8).pow(18));
        assert_eq!(
            config
                .native_to_routable_unit
                .denominator,
            BigUint::from(10u8).pow(18)
        );
    }
}
