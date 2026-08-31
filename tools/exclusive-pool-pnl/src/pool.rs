//! Hardcoded identity of the Fynd exclusive Ekubo V3 pool on Ethereum mainnet.
//!
//! The pool is ETH/USDC behind the `SignedExclusiveSwap` extension: only quotes signed by the
//! Fynd controller key can trade it. Its `PoolKey` carries `fee = 0`, so every unit of LP revenue
//! comes from the extension's per-swap fee rather than from a configured swap fee.

use alloy::primitives::{address, b256, Address, B256};

/// `keccak256(token0 ‖ token1 ‖ poolConfig)` for the pool this tool reports on.
pub const POOL_ID: B256 = b256!("9efd723a29a4e7f40c955f7b968144a5e2a7261a4a0f1573fbb0e2653600e4a4");

/// The `SignedExclusiveSwap` extension that gates and prices every swap on the pool.
pub const EXTENSION: Address = address!("55b703eed01b35641963da2fb2e14885993605a3");

/// Ekubo V3 core, which holds the pool's reserves and emits its swap and fee events.
pub const EKUBO_CORE: Address = address!("00000000000014aa86c5d3c41765bb24e11bd701");

/// Topic0 the extension emits on every pool interaction, with the pool id as topic1.
///
/// Position updates carry this topic too, so a log match alone does not mean a swap happened.
/// [`crate::chain`] confirms a swap by finding Ekubo core's swap log in the same receipt.
pub const INTERACTION_TOPIC: B256 =
    b256!("d59d72ec6d1bba1eef52ec72bd29117451b6c411faada758fdea78bf56a87648");

/// Topic0 of Ekubo core's `FeesAccumulated`, which credits fees to the pool's LPs.
pub const FEES_ACCUMULATED_TOPIC: B256 =
    b256!("f7e050d866774820d81a86ca676f3afe7bc72603ee893f82e99c08fbde39af6c");

/// Block the extension was deployed in — no pool interaction can predate it.
pub const DEPLOY_BLOCK: u64 = 25_648_587;

/// token0 is native ETH.
pub const TOKEN0_DECIMALS: u32 = 18;

/// token1 is USDC.
pub const TOKEN1_DECIMALS: u32 = 6;

/// Binance symbol used as the markout reference price, quoted in token1 per token0.
pub const REFERENCE_SYMBOL: &str = "ETHUSDC";

/// Converts a token0 amount to a float in whole units.
pub fn token0_units(raw: i128) -> f64 {
    raw as f64 / 10f64.powi(TOKEN0_DECIMALS as i32)
}

/// Converts a token1 amount to a float in whole units.
pub fn token1_units(raw: i128) -> f64 {
    raw as f64 / 10f64.powi(TOKEN1_DECIMALS as i32)
}
