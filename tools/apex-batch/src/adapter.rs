//! The bridge between Tycho's simulated pools and APEX's solver types.
//!
//! Every pool enters the batch as `Pool::Apex(PoolMetadata, Arc<dyn ApexPool>)` wrapping a Tycho
//! `ProtocolSim` — no native APEX Uniswap models, no conversion of pool state. That keeps one
//! simulation implementation in play (the one Fynd's baseline also used), so a surplus difference
//! between the two sides is batch clearing and not two different views of the same pool.
//!
//! This mirrors turbine's `TychoApexPool`
//! (`~/Projects/propeller-heads/turbine/src/clearing_algorithm/apex/solver.rs:1114`), with the
//! scaling swapped for the declining [`crate::scaling`] module.

use std::{collections::HashMap, sync::Arc};

use alloy::primitives::Address as AlloyAddress;
use apex_solver::{
    core::{
        pools::custom::ApexPool, Fraction, MarketOrder, PairAddresses, Token as ApexToken,
        TradingPair,
    },
    types::{Address as ApexAddress, U256 as ApexU256},
};
use tycho_simulation::tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim};

use crate::snapshot::BlockBatchSnapshot;

/// A Tycho `ProtocolSim` presented to APEX as a pool.
///
/// APEX's `ApexPool` trait is the only pool interface the batch solver calls, and a blanket impl
/// on `dyn ProtocolSim` is impossible across crates — hence the wrapper. It carries only what the
/// two trait methods need: the token metadata (`ProtocolSim` takes `Token`s, not addresses) and
/// the state itself.
#[derive(Debug, Clone)]
pub struct TychoApexPool {
    /// Tycho's protocol id (`uniswap_v2`, `vm:curve`). Kept for the per-protocol coverage split
    /// the study reports, not read during simulation.
    pub protocol: String,
    /// The pool's tokens by APEX address. `ProtocolSim`'s methods take `Token` values, and the
    /// `ApexPool` trait hands us bare addresses, so the map is the lookup between them.
    pub tokens: HashMap<ApexAddress, Token>,
    /// The simulated pool state at the block being replayed.
    pub pool: Arc<dyn ProtocolSim>,
}

impl ApexPool for TychoApexPool {
    /// The largest amount the pool will sell at `swap_price`, wrapping `ProtocolSim`'s
    /// target-price query.
    ///
    /// Two conversions, both mirroring turbine's wrapper (`solver.rs:1121-1203`), and both easy
    /// to get backwards:
    ///
    /// 1. **Direction.** APEX's `swap_price` is `numerator = sell token, denominator = buy token`;
    ///    `ProtocolSim`'s `Price` is `token_out / token_in`. The two are inverses, so the buy side
    ///    supplies the numerator of the price handed to Tycho.
    /// 2. **Precision.** APEX's prices are already in its 18-decimal space, where a token with
    ///    fewer decimals has a counter-intuitively *larger* price (one unit of USDC is worth 10^12
    ///    atomic units of DAI). Each side of the price is lifted by its own token's scale before
    ///    Tycho sees it, and the returned amount is lifted back on the sell token's.
    fn query_supply(&self, _pair: TradingPair, _swap_price: Fraction) -> ApexU256 {
        todo!(
            "invert the price direction, rescale both sides, and call ProtocolSim::query_pool_swap"
        )
    }

    /// The output for `amount_in`, wrapping `ProtocolSim::get_amount_out`.
    ///
    /// The input is descaled to the token's native decimals *before* the simulation call — Tycho
    /// simulates in native units, and handing it an 18-decimal amount for a 6-decimal token
    /// simulates a trade 10^12 times too large — and the output is scaled back up afterwards.
    /// Returns zero when the pool errors or the result is under `min_amount_out`, which is how
    /// `ApexPool` signals "this pool cannot serve this swap".
    fn get_amount_out(
        &self,
        _token_in: ApexAddress,
        _token_out: ApexAddress,
        _amount_in: ApexU256,
        _min_amount_out: ApexU256,
    ) -> ApexU256 {
        todo!("descale the input, simulate, rescale the output, and floor it at min_amount_out")
    }
}

/// APEX's tâtonnement starting prices, from Fynd's derived per-token price map.
///
/// Fynd prices a token as native units per wei of the gas token; APEX wants an absolute per-token
/// price in its 18-decimal space. The conversion has to fold the token's own decimals in — the
/// same inversion documented on [`TychoApexPool::query_supply`] — or every non-18-decimal token
/// starts the search 10^k off and the batch converges somewhere useless.
///
/// A token whose price is missing, zero, or non-finite is left out: APEX substitutes its
/// configured `starting_price` for unpriced tokens, and the runner counts the omission as
/// `TokenUnpriced` rather than pretending a price existed.
pub fn initial_prices(
    _token_prices: &HashMap<AlloyAddress, f64>,
    _decimals: &HashMap<AlloyAddress, u8>,
) -> HashMap<ApexAddress, ApexU256> {
    todo!("convert each finite, positive Fynd price into an 18-decimal APEX price")
}

/// How an order's limit price is derived — the sensitivity axis the study holds in reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitPolicy {
    /// Use the captured `min_amount_out` as-is: the extracted on-chain floor where there is one,
    /// the capture's synthetic fallback where there is not. The core matrix runs on this.
    AsCaptured,
    /// Override every limit with the executed output less this many basis points. The 50/200 bps
    /// sensitivity band, run only to bound how much the synthetic fallback moves the answer.
    ExecutedLessBps(u32),
}

/// Turn a block's captured trades into APEX market orders, keyed by trading pair.
///
/// Should: for each captured trade, resolve both tokens' [`crate::scaling::TokenScale`], scale
/// `amount_in` and the policy's limit into APEX's 18-decimal space, and push a `MarketOrder`
/// (id = transaction hash, so a clearing can be joined back to its settled trade) onto its
/// pair's bucket. A trade whose limit is absent, whose token has more than 18 decimals, or whose
/// amount overflows scaling is skipped — the caller counts each with its
/// [`crate::runner::ExclusionReason`] so the run reports its own coverage.
pub fn build_orders(
    _snapshot: &BlockBatchSnapshot,
    _decimals: &HashMap<AlloyAddress, u8>,
    _limit_policy: LimitPolicy,
) -> HashMap<PairAddresses, Vec<MarketOrder>> {
    todo!("scale each captured trade into an APEX market order, bucketed by trading pair")
}

/// The APEX token set for a block: one [`ApexToken`] per address the batch's orders and pools
/// touch.
///
/// Should: collect every token referenced by the block's orders and pools, and build an
/// `ApexToken` from Tycho's metadata (address, symbol, decimals). APEX keys `truncate_to_precision`
/// and `validate_result` off these decimals, so a token missing here silently gets defaults.
pub fn apex_tokens(_tokens: &HashMap<AlloyAddress, Token>) -> Vec<ApexToken> {
    todo!("build one APEX token per referenced address from Tycho's token metadata")
}

/// An alloy address as APEX's own 20-byte address type.
///
/// The two are structurally identical but nominally distinct, and APEX's `Address: From<&str>`
/// silently yields the zero address for anything it cannot parse — so conversion goes through the
/// bytes, never through a hex string.
pub fn to_apex_address(address: AlloyAddress) -> ApexAddress {
    ApexAddress(address.into_array())
}

/// An APEX address back as an alloy address, for joining a clearing to its captured trade.
pub fn from_apex_address(address: ApexAddress) -> AlloyAddress {
    AlloyAddress::from(address.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "scaffold: enable with the batch runner"]
    fn test_address_conversion_round_trips() {
        // APEX's `Address: From<&str>` returns the zero address on any parse failure, so a
        // string-based conversion would turn a typo into a real-looking token. Round-tripping
        // through the bytes is the property that rules that out.
        let weth: AlloyAddress = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            .parse()
            .expect("a valid checksummed address");
        assert_eq!(from_apex_address(to_apex_address(weth)), weth);
        assert_ne!(to_apex_address(weth), ApexAddress::default());
    }
}
