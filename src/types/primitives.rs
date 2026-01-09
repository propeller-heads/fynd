//! Primitive types used throughout the solver.

use std::fmt;

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

use super::serde_helpers::biguint_as_string;

/// Unique identifier for a liquidity component.
pub type ComponentId = String;

/// Protocol system identifier matching Tycho Simulation naming.
///
/// Each supported protocol system in Tycho has its own ProtocolSim implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolSystem {
    UniswapV2,
    UniswapV3,
    SushiSwap,
    Curve,
    Balancer,
}

impl From<&str> for ProtocolSystem {
    fn from(system: &str) -> Self {
        match system {
            "uniswap_v2" => ProtocolSystem::UniswapV2,
            "uniswap_v3" => ProtocolSystem::UniswapV3,
            "sushiswap" => ProtocolSystem::SushiSwap,
            "vm:curve" => ProtocolSystem::Curve,
            "vm:balancer" => ProtocolSystem::Balancer,
            _ => ProtocolSystem::Other,
        }
    }
}

impl ProtocolSystem {
    /// Returns the typical gas cost for a swap on this protocol.
    /// These are rough estimates and should be refined based on actual measurements.
    pub fn typical_gas_cost(&self) -> u64 {
        match self {
            ProtocolSystem::UniswapV2 => 100_000,
            ProtocolSystem::UniswapV3 => 150_000,
            ProtocolSystem::SushiSwap => 100_000,
            ProtocolSystem::Curve => 200_000,
            ProtocolSystem::Balancer => 150_000,
        }
    }
}

impl fmt::Display for ProtocolSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolSystem::UniswapV2 => write!(f, "uniswap_v2"),
            ProtocolSystem::UniswapV3 => write!(f, "uniswap_v3"),
            ProtocolSystem::SushiSwap => write!(f, "sushiswap"),
            ProtocolSystem::Curve => write!(f, "curve"),
            ProtocolSystem::Balancer => write!(f, "balancer"),
        }
    }
}

/// Gas price information for transaction cost estimation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasPrice {
    /// Base fee per gas (EIP-1559)
    #[serde(with = "biguint_as_string")]
    pub base_fee: BigUint,
    /// Priority fee per gas (EIP-1559)
    #[serde(with = "biguint_as_string")]
    pub priority_fee: BigUint,
    /// Timestamp when this price was fetched
    pub timestamp_ms: u64,
}

impl GasPrice {
    pub fn new(base_fee: BigUint, priority_fee: BigUint) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self { base_fee, priority_fee, timestamp_ms }
    }

    /// Returns the effective gas price (base + priority).
    pub fn effective_gas_price(&self) -> BigUint {
        &self.base_fee + &self.priority_fee
    }

    /// Check if this gas price is stale (older than threshold).
    pub fn is_stale(&self, max_age_ms: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        now.saturating_sub(self.timestamp_ms) > max_age_ms
    }
}

impl Default for GasPrice {
    fn default() -> Self {
        // Default to 20 gwei base + 1 gwei priority
        Self::new(BigUint::from(20_000_000_000u64), BigUint::from(1_000_000_000u64))
    }
}
