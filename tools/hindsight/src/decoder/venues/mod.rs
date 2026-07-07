//! Per-venue decode strategies.
//!
//! Matching decides *which* transactions are aggregator trades; the strategy
//! decides *how* to recover the user's swap from one. Venue-specific behavior
//! (Relay's fee skim and rebalancing fills, intent-fill maker-finding) lives
//! in its own module so venues evolve independently.

pub(crate) mod intent;
pub(crate) mod metamask;
pub(crate) mod relay;

use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, U256},
    rpc::types::{Log, TransactionReceipt},
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    net::{decode_trade, to_primitive_log, NetSwap, Transfer},
    registry::Registry,
};

sol! {
    /// Emitted by the LiFi Diamond only when an order bridges to another chain (the tuple is
    /// LiFi's `BridgeData`); same-chain LiFi swaps emit `LiFiGenericSwapCompleted` instead.
    event LiFiTransferStarted(
        (bytes32, string, string, address, address, address, uint256, uint256, bool, bool)
            bridgeData
    );
}

/// Whether the transaction started a cross-chain bridge order.
///
/// A bridge deposit is not a same-chain swap: the real output lands on the destination chain,
/// and the trader's only same-chain receipt is a leftover refund. Netting that as a swap pairs
/// the full input with the refund — a phantom trade with an absurd rate. A matching-time
/// concern, like [`select`]: rejected transactions never cost a trace.
pub(crate) fn started_bridge_order(logs: &[Log]) -> bool {
    logs.iter()
        .any(|log| log.topics().first() == Some(&LiFiTransferStarted::SIGNATURE_HASH))
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
pub(crate) fn select<'a>(
    receipt: &'a TransactionReceipt,
    registry: &Registry,
) -> Option<Matched<'a>> {
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
    logs: &[Log],
    native: &[(Address, Address, U256)],
    sender: Address,
    entry_point: Address,
) -> Option<Flow> {
    decode_trade(logs, native, sender)
        .map(|swap| Flow { trader_paid_gas: true, ..Flow::without_fees(sender, swap) })
        .or_else(|| {
            decode_trade(logs, native, entry_point)
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
    logs: &[Log],
    native: &[(Address, Address, U256)],
    sender: Address,
    entry_point: Address,
    fee_collectors: &HashSet<Address>,
) -> Option<Flow> {
    let flow = sender_flow(logs, native, sender, entry_point)?;
    if fee_collectors.contains(&flow.tracked) {
        return Some(flow);
    }
    Some(back_out_client_fees(flow, logs, native, fee_collectors))
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
    logs: &[Log],
    native: &[(Address, Address, U256)],
    fee_collectors: &HashSet<Address>,
) -> Flow {
    let fees = fee_to_collectors(logs, native, fee_collectors);
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

/// Total of each token transferred to a known client fee-collector within the transaction, keyed
/// by token (native ETH is [`Address::ZERO`]).
///
/// Clients skim their fee by sending part of the input token to a fee collector before swapping
/// (so the user's netted `amount_in` includes money that never entered the swap) or part of the
/// output after. Backing the fee out lets the re-solve compare Fynd against the client on the
/// amount actually routed, rather than crediting Fynd with the client's fee. Matches by recipient
/// regardless of sender, so it catches both a direct user skim and a router skim.
fn fee_to_collectors(
    logs: &[Log],
    native_transfers: &[(Address, Address, U256)],
    fee_collectors: &HashSet<Address>,
) -> HashMap<Address, U256> {
    let mut fees: HashMap<Address, U256> = HashMap::new();
    if fee_collectors.is_empty() {
        return fees;
    }
    for &(_, to, value) in native_transfers {
        if fee_collectors.contains(&to) {
            *fees.entry(Address::ZERO).or_default() += value;
        }
    }
    for log in logs {
        let primitive = to_primitive_log(log);
        let Ok(transfer) = Transfer::decode_log(&primitive) else {
            continue;
        };
        if fee_collectors.contains(&transfer.to) {
            *fees.entry(log.address()).or_default() += transfer.value;
        }
    }
    fees
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Log as PrimitiveLog;

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log};

    #[test]
    fn bridge_order_detected() {
        // The LiFi bridge shape (tx 0x72b71802…): 7.2 ETH in, swapped to USDT, 99.5% bridged out,
        // and only the leftover refunded to the trader — flagged by LiFiTransferStarted.
        let diamond = addr(70);
        let primitive = PrimitiveLog::new_unchecked(
            diamond,
            vec![LiFiTransferStarted::SIGNATURE_HASH],
            Default::default(),
        );
        let logs = vec![Log { inner: primitive, ..Default::default() }];
        assert!(started_bridge_order(&logs));

        let swap_logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1000))];
        assert!(!started_bridge_order(&swap_logs));
    }

    #[test]
    fn fee_to_collectors_totals_input_skim() {
        let user = addr(1);
        let router = addr(2);
        let collector = addr(99);
        let token_in = addr(10);
        let pool = addr(50);
        let collectors = HashSet::from([collector]);

        // Router skims part of the input token to the collector; the rest goes to the pool.
        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
        ];
        let fees = fee_to_collectors(&logs, &[], &collectors);
        assert_eq!(fees.get(&token_in).copied(), Some(U256::from(40)));
    }

    #[test]
    fn fee_to_collectors_totals_output_skim() {
        let user = addr(1);
        let pool = addr(50);
        let collector = addr(99);
        let token_out = addr(11);
        let collectors = HashSet::from([collector]);

        // Pool sends the output; part is skimmed to the collector, the rest to the user. The fee
        // map keys this by token_out so the decoder can add it back to the gross swap output.
        let logs = vec![
            make_transfer_log(token_out, pool, collector, U256::from(30)),
            make_transfer_log(token_out, pool, user, U256::from(970)),
        ];
        let fees = fee_to_collectors(&logs, &[], &collectors);
        assert_eq!(fees.get(&token_out).copied(), Some(U256::from(30)));
    }

    #[test]
    fn fee_to_collectors_empty_set_is_noop() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(99), U256::from(40))];
        assert!(fee_to_collectors(&logs, &[], &HashSet::new()).is_empty());
    }
}
