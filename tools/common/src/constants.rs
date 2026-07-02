//! Shared constants used across Fynd tooling.

use alloy::primitives::Address;

/// Native ETH sentinel address used by some DEX aggregators (0xeeee…eeee).
/// Fynd and `eth_call` overrides use `Address::ZERO` for native ETH internally.
pub const ETH_SENTINEL_ADDRESS: Address = Address(alloy::primitives::FixedBytes([0xeeu8; 20]));

/// Native ETH sentinel address as a hex string (with 0x prefix).
/// Used when comparing token addresses given as strings.
pub const ETH_SENTINEL: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// Zero address as a hex string (with 0x prefix). Fynd uses this as the native ETH address.
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
