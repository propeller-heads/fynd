//! Per-venue decode strategies.
//!
//! Matching ([`select`]) decides *which* transactions are aggregator trades and which
//! [`Strategy`] recovers the user's swap from each; [`Strategy::decode`] is the single seam the
//! orchestrator calls, so venue-specific behavior (Relay's fee skim and rebalancing fills,
//! MetaMask's calldata venue declaration, intent-fill maker-finding) lives entirely in this
//! directory and venues evolve independently.
//!
//! Terminology — three tiers, two of which appear in every record:
//! - **client**: the contract the user entered through (`tx.to`) — Relay, MetaMask, a solver's own
//!   router. Order-flow owners; they pick a solver and may skim a fee.
//! - **solver**: the router that computed and settled the route — KyberSwap, 1inch, 0x. These are
//!   Fynd's competitors, and what [`SolverQuote`] quotes. Code symbols and the record schema
//!   historically call this tier `aggregator`; the two words mean the same thing here.
//! - **liquidity venues**: the pools and makers a route executes against (Uniswap, Curve,
//!   prop-AMMs). Not modeled here; they only appear inside traces.
//!
//! A module in this directory is named for the counterparty whose behavior it decodes, which can
//! sit in either of the first two tiers (relay/metamask are clients; kyberswap/paraswap are
//! solvers; lifi's bridge detection guards matching).

pub(crate) mod intent;
pub(crate) mod kyberswap;
pub(crate) mod lifi;
pub(crate) mod metamask;
pub(crate) mod paraswap;
pub(crate) mod relay;

use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::TransactionReceipt,
};
use tracing::debug;

use crate::decoder::{
    ledger::{NetSwap, TransferLedger},
    registry::Registry,
};

/// A solver's own off-chain quote for the swap, recovered from calldata.
///
/// This is the number the client compared against at decision time — what the solver's API
/// promised — as opposed to the settled amount, which is what execution delivered. The fields
/// carry no solver name (the record's `aggregator` column already says who); see
/// [`embedded_quote`] for which solvers declare one and how.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SolverQuote {
    /// Quoted output in `token_out` native units.
    pub amount_out: U256,
    /// The integrator that requested the route (e.g. "relay", "metamask", "Instadapp") — the
    /// true frontend, even when the transaction enters through a wrapper contract. Only some
    /// solvers declare it.
    pub source: Option<String>,
    /// Unix timestamp of the quote, when present. Joined against block time downstream to
    /// separate stale-quote slippage from routing quality.
    pub timestamp: Option<u64>,
}

/// The solver's off-chain quote declared in the transaction's calldata, when the attributed
/// solver is known to embed one. Adding a solver is one match arm.
pub(crate) fn embedded_quote(
    aggregator: &str,
    input: &[u8],
    amount_in: U256,
) -> Option<SolverQuote> {
    match aggregator {
        "kyberswap" => kyberswap::embedded_quote(input),
        "paraswap" => paraswap::embedded_quote(input, amount_in),
        _ => None,
    }
}

/// Whether a quoted output is in the same units as the settled one.
///
/// Quotes are self-reported calldata: integrators sometimes fill them in a different token or
/// decimal basis (seen live: quoted 1.2e23 vs settled 1.2e11), which would fabricate a -100%
/// slippage. A real quote and its settlement differ by slippage, never by orders of magnitude,
/// so anything outside a 2x band is dropped rather than recorded.
pub(crate) fn plausible_quote(quote: &SolverQuote, settled_amount_out: U256) -> bool {
    quote.amount_out <= settled_amount_out.saturating_mul(U256::from(2)) &&
        settled_amount_out <=
            quote
                .amount_out
                .saturating_mul(U256::from(2))
}

/// Everything a decode strategy may need from one matched transaction, so every strategy is
/// called through the same seam ([`Strategy::decode`]) regardless of which inputs it uses.
pub(crate) struct DecodeContext<'a, P> {
    /// RPC access, for strategies that must look beyond the transaction (maker EOA checks).
    pub provider: &'a P,
    pub registry: &'a Registry,
    /// Cross-block contract-code cache, owned by the decoder.
    pub code_cache: &'a mut HashMap<Address, bool>,
    /// The transaction's flattened value movements.
    pub ledger: &'a TransferLedger,
    /// Root calldata of the transaction (venue declarations, embedded quotes).
    pub input: &'a [u8],
    pub sender: Address,
    pub entry_point: Address,
}

/// How to recover the swap from a matched transaction.
pub(crate) enum Strategy {
    /// The sender is the trader: net its flow (direct aggregator swaps).
    Sender,
    /// The trader is an order maker, not the sender: either the tx was
    /// discovered via a known aggregator log (`tx.to` is a rotating filler) or
    /// `tx.to` is a batch settler entered by a solver.
    Maker,
    /// Relay client entry: sender netting with client-fee back-out, falling
    /// back to solver-rebalance decoding.
    Relay,
    /// MetaMask Swap Router entry: sender netting with client-fee back-out.
    Metamask,
}

impl Strategy {
    /// Recover the user flow from a matched transaction. Each variant owns its venue's quirks;
    /// the orchestrator only sequences.
    pub(crate) async fn decode<P: Provider>(&self, ctx: DecodeContext<'_, P>) -> Option<Flow> {
        match self {
            Self::Sender => sender_flow(ctx.ledger, ctx.sender, ctx.entry_point),
            Self::Maker => {
                intent::find_maker_trade(
                    ctx.provider,
                    ctx.ledger,
                    &[ctx.entry_point, ctx.sender],
                    ctx.registry,
                    ctx.code_cache,
                )
                .await
            }
            Self::Relay => relay::decode(ctx.ledger, ctx.sender, ctx.entry_point, ctx.registry),
            Self::Metamask => metamask::decode(ctx.ledger, ctx.sender, ctx.input, ctx.registry),
        }
    }

    /// Whether the entry point is a client wrapper around the settling solver (Relay, MetaMask).
    /// The wrapper's own gas is charged whichever solver the client picks, so gas accounting
    /// reads the route's trace frame instead of the whole receipt.
    pub(crate) fn routes_via_wrapper(&self) -> bool {
        match self {
            Self::Relay | Self::Metamask => true,
            Self::Sender | Self::Maker => false,
        }
    }
}

/// A matched transaction and the strategy to decode it.
pub(crate) struct Matched<'a> {
    pub receipt: &'a TransactionReceipt,
    pub entry_point: Address,
    pub strategy: Strategy,
}

/// The decoded user flow of a matched transaction.
pub(crate) struct Flow {
    /// The address whose net flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Client fee skimmed from the input token, already backed out of
    /// `swap.amount_in`.
    pub client_fee: Option<U256>,
    /// Client fee skimmed from the output token, already added back into
    /// `swap.amount_out`.
    pub client_fee_out: Option<U256>,
    /// Venue label asserted by the strategy itself (e.g. MetaMask declares its
    /// venue in calldata), overriding trace-based attribution.
    pub aggregator_override: Option<String>,
    /// Whether the tracked trader sent the transaction and therefore paid its gas. Decides if the
    /// settled route's gas may be charged against the settled output — a maker or a
    /// solver-rebalance trader had its gas paid by someone else, so nothing is deducted there.
    pub trader_paid_gas: bool,
}

impl Flow {
    fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self {
            tracked,
            swap,
            client_fee: None,
            client_fee_out: None,
            aggregator_override: None,
            trader_paid_gas: false,
        }
    }
}

/// Match a receipt and choose its decode strategy.
///
/// A transaction qualifies two ways: its entry point (`tx.to`) is a known
/// client or aggregator, or one of its logs was emitted by a known aggregator
/// (filler-initiated intent fills, where `tx.to` is a rotating filler).
/// Matched transactions that start a cross-chain bridge order are vetoed here
/// (see [`lifi::started_bridge_order`]), before they cost a trace.
pub(crate) fn select<'a>(
    receipt: &'a TransactionReceipt,
    registry: &Registry,
) -> Option<Matched<'a>> {
    let matched = match_entry(receipt, registry)?;
    if lifi::started_bridge_order(matched.receipt.logs()) {
        debug!(
            tx = %matched.receipt.transaction_hash,
            client = %registry.label(matched.entry_point),
            "cross-chain bridge order; skipping"
        );
        return None;
    }
    Some(matched)
}

/// The strategy for a receipt's entry point, before any veto.
fn match_entry<'a>(receipt: &'a TransactionReceipt, registry: &Registry) -> Option<Matched<'a>> {
    if !receipt.status() {
        return None;
    }
    let entry_point = receipt.to?;
    if registry
        .relay()
        .routers
        .contains(&entry_point)
    {
        return Some(Matched { receipt, entry_point, strategy: Strategy::Relay });
    }
    if entry_point == registry.metamask().router {
        return Some(Matched { receipt, entry_point, strategy: Strategy::Metamask });
    }
    if registry.is_known(entry_point) {
        // Batch settlers (e.g. CoW) are entered by a solver, not the trader, so the real swap is
        // an order maker's net flow — decode it like a filler-initiated intent fill.
        let strategy =
            if registry.is_batch_settler(entry_point) { Strategy::Maker } else { Strategy::Sender };
        return Some(Matched { receipt, entry_point, strategy });
    }
    let via_log = receipt
        .logs()
        .iter()
        .any(|log| registry.is_aggregator(log.address()));
    via_log.then_some(Matched { receipt, entry_point, strategy: Strategy::Maker })
}

/// Net the sender's flow, falling back to the entry point for the rare case
/// where output is delivered there.
///
/// A sender-tracked flow marks the trader as the gas payer; the entry-point fallback does not
/// (the tracked address and the gas-paying sender differ, so charging the gas is not clear-cut).
pub(crate) fn sender_flow(
    ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<Flow> {
    ledger
        .net_swap(sender)
        .map(|swap| Flow { trader_paid_gas: true, ..Flow::without_fees(sender, swap) })
        .or_else(|| {
            ledger
                .net_swap(entry_point)
                .map(|swap| Flow::without_fees(entry_point, swap))
        })
}

/// Net the sender's flow and back the client's fee skim out of it — the shared shape of every
/// fee-skimming client entry (Relay, MetaMask).
///
/// One exception: when the tracked trader IS a fee collector, the transaction is a treasury
/// operation — the collector's receipts are its own output, not a skim, and backing them "out"
/// would add the output to itself and double it.
pub(super) fn client_fee_flow(
    ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
    fee_collectors: &HashSet<Address>,
) -> Option<Flow> {
    let flow = sender_flow(ledger, sender, entry_point)?;
    if fee_collectors.contains(&flow.tracked) {
        return Some(flow);
    }
    Some(back_out_client_fees(flow, ledger, fee_collectors))
}

/// Back a client-fee skim out of a decoded user flow.
///
/// The collector can skim on either side. An input-side skim is subtracted
/// from `amount_in` (the user's gross spend included money that never entered
/// the swap) and an output-side skim is added back into `amount_out` (the
/// swap produced more than the user kept), so both sides are the amounts
/// actually swapped — the like-for-like basis vs Fynd.
fn back_out_client_fees(
    flow: Flow,
    ledger: &TransferLedger,
    fee_collectors: &HashSet<Address>,
) -> Flow {
    let fees = ledger.received_by(fee_collectors);
    let client_fee = fees
        .get(&flow.swap.token_in)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_in =
        client_fee.map_or(flow.swap.amount_in, |fee| flow.swap.amount_in.saturating_sub(fee));
    let client_fee_out = fees
        .get(&flow.swap.token_out)
        .copied()
        .filter(|fee| !fee.is_zero());
    let amount_out =
        client_fee_out.map_or(flow.swap.amount_out, |fee| flow.swap.amount_out.saturating_add(fee));
    Flow {
        tracked: flow.tracked,
        swap: NetSwap { amount_in, amount_out, ..flow.swap },
        client_fee,
        client_fee_out,
        aggregator_override: flow.aggregator_override,
        trader_paid_gas: flow.trader_paid_gas,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    #[test]
    fn client_fee_flow_backs_out_input_skim() {
        // Router skims part of the input token to the collector; the rest goes to the pool.
        let user = addr(1);
        let router = addr(2);
        let collector = addr(99);
        let token_in = addr(10);
        let token_out = addr(11);
        let pool = addr(50);
        let collectors = HashSet::from([collector]);

        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = client_fee_flow(&ledger, user, router, &collectors).unwrap();
        assert_eq!(flow.swap, swap(token_in, 960, token_out, 2000));
        assert_eq!(flow.client_fee, Some(U256::from(40)));
        assert_eq!(flow.client_fee_out, None);
    }

    #[test]
    fn client_fee_flow_keeps_fee_free_trade_unchanged() {
        // Nothing reached a fee wallet, nothing is backed out.
        let user = addr(1);
        let pool = addr(50);
        let collectors = HashSet::from([addr(99)]);

        let logs = vec![
            make_transfer_log(addr(10), user, pool, U256::from(1000)),
            make_transfer_log(addr(11), pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);

        let flow = client_fee_flow(&ledger, user, pool, &collectors).unwrap();
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
        assert_eq!(flow.client_fee, None);
        assert_eq!(flow.client_fee_out, None);
    }

    #[test]
    fn plausible_quote_accepts_slippage_and_rejects_unit_mismatch() {
        let quote = |amount: u128| SolverQuote {
            amount_out: U256::from(amount),
            source: None,
            timestamp: None,
        };
        // The audited Relay+KyberSwap trade: quoted 70,400.41, settled 69,996.28 — 57bps of
        // slippage, kept.
        assert!(plausible_quote(&quote(70_400_409_935), U256::from(69_996_280_564u64)));
        // Seen live via Instadapp: quoted in 18-decimal units, settled in 6 — dropped.
        assert!(!plausible_quote(
            &quote(120_001_117_253_254_637_416_284),
            U256::from(120_000_000_000u64)
        ));
    }

    #[test]
    fn embedded_quote_dispatches_by_venue() {
        // A ParaSwap-shaped word triple only parses when the attributed venue is paraswap;
        // an unlisted venue never yields a quote from the same bytes.
        let amount_in = U256::from(171_521_496u64);
        let mut input = vec![0xe3u8, 0xea, 0xd5, 0x9e];
        for word in [amount_in, U256::from(171_430_663u64), U256::from(171_602_266u64), U256::ZERO]
        {
            input.extend_from_slice(&word.to_be_bytes::<32>());
        }
        assert!(embedded_quote("paraswap", &input, amount_in).is_some());
        assert!(embedded_quote("1inch", &input, amount_in).is_none());
    }
}
