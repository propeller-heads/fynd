use std::str::FromStr;

use num_bigint::BigUint;
use tycho_simulation::{
    tycho_common::models::{Chain, NativeAsset},
    tycho_core::{models::Address, simulation::protocol_sim::Price},
};

/// How a chain's native gas is priced.
///
/// Gas is paid in the chain's *native* asset: `gas * gas_price` counts its raw units. Token prices
/// are measured against the *routable* gas token, an ERC-20 that is worth exactly the native asset
/// and that pools hold. On most chains it is the wrapper (WETH), with the native asset's decimals.
/// On a *shared-balance* chain such as Arc, it is a second ERC-20 interface over the same balance
/// as the native asset, with its own decimals: native USDC pays gas in 18-decimal raw units, while
/// pools trade it at 6.
///
/// Derived token prices are stored in token raw units per native raw unit, so gas costs convert
/// without a further scale. [`Self::to_routable_units`] converts them for consumers that quote
/// against the routable token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GasTokenConfig {
    /// Address of the routable gas token, where every pricing route starts and ends.
    address: Address,
    /// Routable-token raw units each pricing probe buys with: one whole routable token.
    probe_amount: BigUint,
    /// Routable-token raw units worth one native raw unit: `1/1` on wrapper chains, `10^6/10^18`
    /// on Arc.
    routable_per_native_unit: Price,
}

impl GasTokenConfig {
    /// An 18-decimal wrapper at `address`. Used for the unconfigured default and by the
    /// deprecated address-only setter, which priced every chain this way.
    pub(crate) fn wrapper(address: Address) -> Self {
        Self {
            address,
            probe_amount: BigUint::from(10u8).pow(18),
            routable_per_native_unit: Price {
                numerator: BigUint::from(1u8),
                denominator: BigUint::from(1u8),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        address: Address,
        probe_amount: BigUint,
        routable_per_native_unit: Price,
    ) -> Self {
        Self { address, probe_amount, routable_per_native_unit }
    }

    /// Returns the routable gas token's address.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Returns the routable-token raw units each pricing probe buys with.
    pub(crate) fn probe_amount(&self) -> &BigUint {
        &self.probe_amount
    }

    /// Returns the routable-token raw units worth one native raw unit.
    pub(crate) fn routable_per_native_unit(&self) -> &Price {
        &self.routable_per_native_unit
    }

    /// Converts a rate in token raw units per routable raw unit into token raw units per native
    /// raw unit. The pricing pass applies it to every rate it measures.
    pub(crate) fn to_native_units(&self, rate: Price) -> Price {
        let scale = &self.routable_per_native_unit;
        Price {
            numerator: rate.numerator * &scale.numerator,
            denominator: rate.denominator * &scale.denominator,
        }
    }

    /// Converts a derived price in token raw units per native raw unit into token raw units per
    /// routable raw unit. The exact inverse of the pricing pass's `to_native_units`.
    pub fn to_routable_units(&self, price: &Price) -> Price {
        let scale = &self.routable_per_native_unit;
        Price {
            numerator: &price.numerator * &scale.denominator,
            denominator: &price.denominator * &scale.numerator,
        }
    }
}

/// Error returned when a chain is not supported.
#[derive(Debug, Clone, thiserror::Error)]
#[error("routable gas token not configured for chain: {chain:?}")]
pub struct UnsupportedChainError {
    pub(crate) chain: Chain,
}

impl UnsupportedChainError {
    /// Returns the unsupported chain.
    pub fn chain(&self) -> Chain {
        self.chain
    }
}

/// Resolves how `chain`'s native gas is priced, resolving custom chains from the registry.
///
/// Wrapper chains price against their wrapper at `1/1`. Shared-balance chains such as Arc price
/// against their routable representation, scaled by the two tokens' decimals.
///
/// # Errors
/// Returns `UnsupportedChainError` if the chain (custom) is not registered, or if its native asset
/// has no routable token (Starknet). Such a chain fails the solver build rather than getting a
/// placeholder gas token.
pub fn gas_token_config(chain: &Chain) -> Result<GasTokenConfig, UnsupportedChainError> {
    let unsupported = || UnsupportedChainError { chain: *chain };
    match chain
        .try_native_asset()
        .map_err(|_| unsupported())?
    {
        NativeAsset::Wrapper { native: _, wrapper } => Ok(GasTokenConfig {
            probe_amount: BigUint::from(10u8).pow(wrapper.decimals),
            routable_per_native_unit: Price {
                numerator: BigUint::from(1u8),
                denominator: BigUint::from(1u8),
            },
            address: wrapper.address,
        }),
        NativeAsset::SharedBalance { native, routable } => Ok(GasTokenConfig {
            probe_amount: BigUint::from(10u8).pow(routable.decimals),
            routable_per_native_unit: Price {
                numerator: BigUint::from(10u8).pow(routable.decimals),
                denominator: BigUint::from(10u8).pow(native.decimals),
            },
            address: routable.address,
        }),
        NativeAsset::NativeOnly { native: _ } => Err(unsupported()),
    }
}

/// Returns the routable gas token address for `chain`: the wrapper (WETH) on most chains, the
/// shared-balance ERC-20 on Arc. Never the native asset's own address.
///
/// # Errors
/// Returns `UnsupportedChainError` under the same conditions as [`gas_token_config`].
pub fn gas_token(chain: &Chain) -> Result<Address, UnsupportedChainError> {
    gas_token_config(chain).map(|config| config.address)
}

/// Returns the routable gas token address for `chain`.
///
/// # Errors
/// Returns `UnsupportedChainError` under the same conditions as [`gas_token_config`].
#[deprecated(note = "returns the routable gas token, not the native asset; use `gas_token`")]
pub fn native_token(chain: &Chain) -> Result<Address, UnsupportedChainError> {
    gas_token(chain)
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

    fn one() -> Price {
        Price { numerator: BigUint::from(1u8), denominator: BigUint::from(1u8) }
    }

    #[test]
    fn test_gas_token_config_arc() {
        let config = gas_token_config(&Chain::Arc).unwrap();

        assert_eq!(
            config.address(),
            &Bytes::from_str("0x3600000000000000000000000000000000000000").unwrap()
        );
        assert_eq!(config.probe_amount(), &BigUint::from(1_000_000u64));
        assert_eq!(
            config.routable_per_native_unit(),
            &Price {
                numerator: BigUint::from(1_000_000u64),
                denominator: BigUint::from(10u8).pow(18),
            }
        );
        assert_eq!(&gas_token(&Chain::Arc).unwrap(), config.address());
    }

    #[test]
    fn test_gas_token_config_ethereum() {
        let config = gas_token_config(&Chain::Ethereum).unwrap();

        assert_eq!(
            config.address(),
            &Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
        );
        assert_eq!(config.probe_amount(), &BigUint::from(10u8).pow(18));
        assert_eq!(config.routable_per_native_unit(), &one());
    }

    #[test]
    fn test_gas_token_config_starknet() {
        assert!(gas_token_config(&Chain::Starknet).is_err());
        assert!(gas_token(&Chain::Starknet).is_err());
    }

    #[test]
    fn test_gas_token_config_arc_round_trip() {
        let config = gas_token_config(&Chain::Arc).unwrap();
        let rate = Price { numerator: BigUint::from(3u8), denominator: BigUint::from(7u8) };

        let native = config.to_native_units(rate.clone());
        let routable = config.to_routable_units(&native);

        // 3/7 token raw units per USDC raw unit is 3 * 10^-12 / 7 per native raw unit.
        assert_eq!(
            native,
            Price {
                numerator: BigUint::from(3_000_000u64),
                denominator: BigUint::from(7u8) * BigUint::from(10u8).pow(18),
            }
        );
        assert_eq!(
            &routable.numerator * &rate.denominator,
            &rate.numerator * &routable.denominator
        );
    }

    #[test]
    fn test_gas_token_config_wrapper_identity() {
        let config = gas_token_config(&Chain::Ethereum).unwrap();
        let rate = Price { numerator: BigUint::from(3u8), denominator: BigUint::from(7u8) };

        assert_eq!(config.to_native_units(rate.clone()), rate);
        assert_eq!(config.to_routable_units(&rate), rate);
    }
}
