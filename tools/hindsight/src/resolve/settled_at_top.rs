//! Re-simulating the settled swap on the same state Fynd's quote saw.
//!
//! **Sketch. Nothing here runs yet** — every entry point returns
//! [`NotSimulated::NotImplemented`], so the monitor is unchanged. The shape is here to be argued
//! with before it is built.
//!
//! # The problem this measures away
//!
//! Today a comparison pairs Fynd's quote, solved at top-of-block (state N-1), against what the
//! settled swap *actually paid*, which executed at its own position inside block N. The two see
//! different pool state, so every difference mixes two effects: our route being better or worse,
//! and the block moving underneath the trade. `Sandwiched` and the back-of-block `Slippage`
//! measurement exist to bound the second effect rather than remove it.
//!
//! Re-simulating the settled transaction against state N-1 removes it. Both sides then price the
//! same pools at the same instant, and questions about intra-block drift, transaction position and
//! MEV stop applying.
//!
//! # What it costs, measured
//!
//! Ethereum block 25741805, 17 decoded trades, Alchemy:
//!
//! | call | time |
//! |---|---|
//! | `debug_traceBlockByNumber`, what the decoder already does per block | 1,561 ms |
//! | all 17 settled swaps re-simulated at N-1, one JSON-RPC batch | 570 ms |
//! | one `debug_traceCall` on its own | ~245 ms |
//!
//! One batched request per block, not one request per trade. `callTracer` with
//! `tracerConfig: {withLog: true}` returns the logs inside the trace tree, which is what
//! the decoder's `TransferLedger` already consumes, and the call itself is built from the root
//! frame the block trace already carries — so nothing new is fetched beyond the batch.
//!
//! # Whether the delay fits, per chain
//!
//! The batch is one round trip whose size scales with trades per block, so Ethereum's 570 ms is the
//! **worst** case rather than a constant: a chain with a handful of aggregator trades per block
//! costs about one call (~245 ms, latency rather than count), and a block with no decoded trades
//! costs nothing extra.
//!
//! Block times from `Chain::try_block_time_secs` (tycho-common 0.363.0), against the seven address
//! books in `registry::BUILTIN_CHAINS`. Only Ethereum is measured — both cost drivers, trades per
//! block and node latency, are per chain.
//!
//! | chain | block time | measured | headroom |
//! |---|---|---|---|
//! | ethereum | 12 s | 2.1 s for 17 trades | spare |
//! | base | 2 s | not measured | tight |
//! | polygon | 2 s | not measured | tight |
//! | arbitrum | 1 s | not measured | none |
//! | bsc | 1 s | not measured | none |
//! | unichain | 1 s | not measured | none |
//! | robinhood | 1 s | not measured | none |
//!
//! Having no headroom does not make this infeasible on those chains. The monitor is already a
//! sampling one: `--max-lag-blocks` exists because it cannot keep every block on a fast chain, and
//! it skips blocks on Ethereum too — staging covered 3,183 distinct blocks against prod's 3,415
//! over the same range. So the cost of this feature outside Ethereum is **sample rate**, not
//! correctness. Roughly a third fewer blocks covered, against a comparison with no intra-block
//! drift in it.
//!
//! Whether that trade is worth taking depends on how many trades a day each chain needs for a
//! stable number, which is why this should be **opt-in per chain**.
//!
//! # Open questions
//!
//! - **Which number is the headline.** This one answers "was our route better than theirs on equal
//!   state", which is a routing-quality measure. The existing one answers "what would this trader
//!   have kept", which is a real-money counterfactual and needs the amount actually paid. They are
//!   different claims, so this should be a third measurement beside `top` and `back`, not a
//!   replacement.
//! - **The transactions that cannot be re-simulated.** 3 of 17 in the measured block: one `ERC20:
//!   transfer amount exceeds allowance` (the approval was granted earlier in block N), one `out of
//!   gas` (the probe passed the transaction's own gas limit), one reverted with no reason. The
//!   first two are fixable — see [`simulate_at_top`] — leaving about 1 in 17. What must not be
//!   silently dropped is a swap whose own `min_amount_out` could not be met at N-1: that means the
//!   fill only worked *because* earlier transactions moved the pool, and excluding those flatters
//!   our number. [`NotSimulated`] keeps them apart so they can be counted.
//! - **Summing across a block.** Each call runs independently against N-1, so two trades hitting
//!   one pool both see it untouched. Fynd's quotes already behave that way, so the comparison stays
//!   consistent, but a per-block sum of savings now double-counts the same liquidity on both sides.

use alloy::{
    primitives::{Address, U256},
    rpc::types::trace::geth::CallFrame,
};

use crate::decoder::DecodedTrade;

/// What the settled swap would have paid on the state Fynd's top-of-block quote saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SettledAtTop {
    /// The amount of `token_out` the re-simulated transaction paid the recipient — recovered the
    /// same way the declared decode recovers it from the real transaction, so the two figures are
    /// read by one rule and differ only in the state they ran against.
    pub amount_out: U256,
    /// Gas the re-simulation burned. Recorded to compare against the real receipt, not to charge:
    /// every comparison hindsight makes is gross of gas.
    pub gas_used: u64,
}

/// Why a settled swap produced no top-of-block figure.
///
/// Separate variants because they mean different things for the metric. `Reverted` with a
/// floor-shaped reason is the one that biases the result, so it must be countable on its own rather
/// than folded into a single "failed" bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "sketch: the variants are the proposal; the code that returns them lands with the \
              implementation"
)]
pub(crate) enum NotSimulated {
    /// This module is a sketch. Every entry point returns this until it is built.
    NotImplemented,
    /// The transaction reverted on state N-1, carrying the node's reason when it gave one.
    ///
    /// Two reasons are the caller's own fault and are retried rather than recorded: an allowance
    /// or Permit2 nonce that a transaction earlier in block N had set, and an out-of-gas from
    /// reusing the transaction's own gas limit.
    Reverted(String),
    /// The re-simulation succeeded but the recipient received none of `token_out`, so there is no
    /// output to compare. Distinct from a revert: the transaction ran.
    OutputNotFound,
    /// The node refused or failed the call.
    RpcError(String),
}

/// One decoded trade and the transaction that settled it.
///
/// The root frame comes from the block trace `Decoder::decode_block` already fetched, and carries
/// `from`, `to`, `value`, `input` and `gas` — everything the re-simulation needs. Threading it out
/// of the decoder is the one plumbing change this feature needs, and it costs no extra RPC call.
#[expect(
    dead_code,
    reason = "sketch: read by the batch builder, which lands with the implementation"
)]
pub(crate) struct SettledCall<'a> {
    pub trade: &'a DecodedTrade,
    pub root: &'a CallFrame,
}

/// Re-simulate every trade of one block against the parent block's state, in a single batched
/// request, and return one outcome per trade in the order given.
///
/// Planned shape of each element of the batch:
///
/// ```text
/// debug_traceCall(
///   { from, to, value, data, gas: GAS_CAP },
///   block_number - 1,
///   { tracer: "callTracer", tracerConfig: { withLog: true }, stateOverrides: ... },
/// )
/// ```
///
/// `GAS_CAP` replaces the transaction's own limit, which is set for the state it really ran
/// against and was already seen to run out at N-1. The state overrides cover the other avoidable
/// failure: an ERC-20 allowance or Permit2 nonce that a transaction earlier in block N had set,
/// which is not yet in place at N-1.
#[expect(
    clippy::unused_async,
    dead_code,
    reason = "sketch: the signature is the proposal, the body lands with the implementation"
)]
pub(crate) async fn simulate_at_top(
    calls: &[SettledCall<'_>],
    block_number: u64,
) -> Vec<Result<SettledAtTop, NotSimulated>> {
    let _ = block_number;
    calls
        .iter()
        .map(|_| Err(NotSimulated::NotImplemented))
        .collect()
}

/// Read the output out of one re-simulated trace, by the same rule the real transaction's output is
/// read by: the amount of `token_out` the recipient received.
///
/// Reusing that rule is the point — a difference between the two figures then means the state
/// differed, never that the two were measured differently.
///
/// The rule lives in `declared::recover_output`, and both it and `TransferLedger` are private to
/// `decoder`. So this function probably belongs in `decoder` and gets called from here, rather than
/// `decoder` widening its exports. That is a decision for the implementation, and the reason this
/// signature takes the trace rather than a ledger.
#[expect(
    dead_code,
    reason = "sketch: the signature is the proposal, the body lands with the implementation"
)]
pub(crate) fn recover_simulated_output(
    trade: &DecodedTrade,
    recipient: Address,
    simulated: &CallFrame,
) -> Result<U256, NotSimulated> {
    let _ = (trade, recipient, simulated);
    Err(NotSimulated::NotImplemented)
}

#[cfg(test)]
mod tests {

    /// The behaviour to assert once this is built: the re-simulated output is read by the same
    /// rule as the real one, so a trade whose pools did not move between N-1 and its own position
    /// in block N produces the same figure twice.
    #[test]
    #[ignore = "sketch: no implementation to test yet"]
    fn test_unmoved_pools_give_the_settled_amount() {
        unreachable!("pending implementation")
    }

    /// A swap whose own floor cannot be met at N-1 must surface as `Reverted`, not as a missing
    /// record: it is the one failure that biases the metric, so the count has to be visible.
    #[test]
    #[ignore = "sketch: no implementation to test yet"]
    fn test_floor_unfillable_at_top_is_reported_not_dropped() {
        unreachable!("pending implementation")
    }

    /// An allowance a transaction earlier in the block had granted is supplied by a state
    /// override, so it is not counted as a failure to simulate.
    #[test]
    #[ignore = "sketch: no implementation to test yet"]
    fn test_allowance_set_earlier_in_the_block_is_overridden() {
        unreachable!("pending implementation")
    }

    /// A per-block sum must not double-count a pool two trades both consumed, since each call
    /// runs against the same untouched state.
    #[test]
    #[ignore = "sketch: no implementation to test yet"]
    fn test_two_trades_on_one_pool_are_not_summed_naively() {
        unreachable!("pending implementation")
    }
}
