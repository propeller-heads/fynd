use std::str::FromStr;

use tycho_simulation::{tycho_common::models::Chain, tycho_core::models::Address};

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

/// Returns the wrapped native token address for the given chain, resolving custom chains from the
/// registry.
///
/// # Errors
/// Returns `UnsupportedChainError` if the chain (custom) is not registered, or if it has no
/// wrapper contract for its native asset (Starknet, or a chain whose native balance is itself a
/// routable token). `native_token` feeds the solver's gas token, so such a chain fails the build
/// rather than getting a placeholder gas token.
pub fn native_token(chain: &Chain) -> Result<Address, UnsupportedChainError> {
    let unsupported = || UnsupportedChainError { chain: *chain };
    chain
        .try_wrapped_native_token()
        .map_err(|_| unsupported())?
        .map(|token| token.address)
        .ok_or_else(unsupported)
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
