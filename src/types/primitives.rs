//! Primitive types used throughout the solver.

use std::fmt;

use serde::{Deserialize, Serialize};

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

/// Error when parsing an unknown protocol system.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown protocol system: {0}")]
pub struct UnknownProtocolSystem(pub String);

impl TryFrom<&str> for ProtocolSystem {
    type Error = UnknownProtocolSystem;

    fn try_from(system: &str) -> Result<Self, Self::Error> {
        match system {
            "uniswap_v2" => Ok(ProtocolSystem::UniswapV2),
            "uniswap_v3" => Ok(ProtocolSystem::UniswapV3),
            "sushiswap" => Ok(ProtocolSystem::SushiSwap),
            "vm:curve" => Ok(ProtocolSystem::Curve),
            "vm:balancer" => Ok(ProtocolSystem::Balancer),
            _ => Err(UnknownProtocolSystem(system.to_string())),
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
    /// Returns the raw protocol system name as expected by tycho-execution encoder.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolSystem::UniswapV2 => write!(f, "uniswap_v2"),
            ProtocolSystem::UniswapV3 => write!(f, "uniswap_v3"),
            ProtocolSystem::SushiSwap => write!(f, "sushiswap"),
            ProtocolSystem::Curve => write!(f, "vm:curve"),
            ProtocolSystem::Balancer => write!(f, "vm:balancer"),
        }
    }
}
