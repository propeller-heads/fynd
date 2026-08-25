//! Vetoes: rejections that keep non-trades out of the records, and the `Veto` type they all
//! share.
//!
//! There are two veto points. A solver vetoes from its own `SolverDecoder::declared`, which the
//! settling solver answers before anything is decoded. The checks in this module run after
//! decoding: netting can pair value legs that were never a swap — the payment side of an
//! NFT purchase, or a cross-chain deposit's dust refund. Each check recognizes one such shape
//! from the transaction itself (no prices, no external data); `check` is the single entry
//! point the decoder runs on every decoded flow, so adding a check never touches the
//! orchestrator.

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};

use crate::decoder::{
    registry::Registry,
    transfer_ledger::{SettledSwap, Transfer, TransferLedger, RESIDUE_GROSS_RATIO},
};

/// A transaction rejected as not a comparable trade, by the shape that disqualified it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Veto {
    /// The trader received an NFT: the netted token flow is the payment side of a purchase
    /// (e.g. an NFT sweep through Relay + Seaport), and the real consideration is invisible to
    /// ERC-20 netting — recording it would pair the payment with the change as a swap that never
    /// happened.
    NftPurchase,
    /// A native <-> wrapped-native "swap" far off 1:1, which a wrap or unwrap cannot produce: a
    /// mis-paired cross-chain deposit whose only same-chain receipt is a dust remainder refund.
    MispairedWrapPair,
    /// A cross-chain bridge order settled by a solver router: the real output lands on the
    /// destination chain, so there is no same-chain swap to record. Returned by the settling
    /// solver's own `SolverDecoder::declared`, never by `check`.
    BridgeOrder,
    /// The settling solver's calldata named the output token and the address it is paid to, but
    /// that address received none of it, or less than the floor the same calldata enforces. The
    /// trade cannot be read from the source that named it, and netting would answer a different
    /// question, so the transaction is dropped. Returned by `declared::declared_flow`.
    OutputNotFound,
    /// The settling solver's calldata fixed the output and only bounded the input, and the sender
    /// paid none of the input token, or more than that bound. Same reasoning as `OutputNotFound`
    /// on the other side of the trade. Returned by `declared::declared_flow`.
    InputNotFound,
    /// Part of the trade's value was taken by a fee on transfer: the token contract (or an
    /// unregistered fee) split a transfer, landing a significant share on an address that only
    /// accumulates — its fee wallet. Selling such a token, the fee nets into `amount_in`;
    /// buying it, the trader's receipt is already net of the fee the pool paid gross.
    /// Fee-on-transfer tokens are not supported by the Tycho simulation (the re-solve quotes as
    /// if the full amounts moved fee-free), so every comparison would credit Fynd with the
    /// token's own fee.
    FeeOnTransfer,
}

/// A shortfall this far below the declared input is the token taking a cut, not rounding: one
/// basis point of the amount.
const TAX_DETECTION_BPS: u64 = 10_000;

/// Whether the trader delivered less of the input token than the trade's own `amount_in` says.
///
/// A normal ERC-20 transfer of X delivers X, so a shortfall means the token took a cut in transit.
/// Tycho labels these tokens quality 100 and quotes them as if transfers were free, so a re-solve
/// computes the output for the full amount while the pools only ever received the taxed
/// remainder — the comparison would credit Fynd with the token's own fee. Seen on `EverRise`
/// (RISE), which delivered 0.95 of every 1.0 its Universal Router calldata authorised.
///
/// A trader who sent none of the input token funded the swap some other way (a solver rebalance,
/// a third-party funder), which this cannot judge, so it is not a veto.
fn input_short_of_declared(flow: &SettledSwap, transfer_ledger: &TransferLedger) -> bool {
    let sent = transfer_ledger.sent_by_address(flow.tracked, flow.token_in);
    if sent.is_zero() || sent >= flow.amount_in {
        return false;
    }
    let shortfall = flow.amount_in - sent;
    shortfall.saturating_mul(U256::from(TAX_DETECTION_BPS)) >= flow.amount_in
}

/// Check a decoded flow against every post-decode veto, returning the first.
///
/// `payee` is the address the flow's output was anchored on — the declared output recipient, or
/// the tracked trader when nothing named one. The fee-on-transfer test needs it so a named payee
/// is not mistaken for a token's fee wallet.
pub(crate) fn check(
    flow: &SettledSwap,
    transfer_ledger: &TransferLedger,
    logs: &[Log],
    registry: &Registry,
    payee: Address,
) -> Option<Veto> {
    if received_nft(logs, flow.tracked) {
        return Some(Veto::NftPurchase);
    }
    if wrap_pair_mispaired(flow, registry.wrapped_native()) {
        return Some(Veto::MispairedWrapPair);
    }
    if input_short_of_declared(flow, transfer_ledger) {
        return Some(Veto::FeeOnTransfer);
    }
    if fee_on_transfer(flow, transfer_ledger, registry, payee) {
        return Some(Veto::FeeOnTransfer);
    }
    None
}

/// Whether a fee on transfer took a significant share of either side of the trade.
///
/// Catches any fee-on-transfer token that Tycho mislabels and does not support: the re-solve
/// quotes fee-free amounts, so the comparison would credit Fynd with the token's own fee.
/// Registered venue fee collectors are exempt — their fees are backed out downstream. A leg
/// under the residue line (1% of the trade amount, `RESIDUE_GROSS_RATIO`) is dust, not a fee.
///
/// `payee` is exempt too: the address the trade's output was anchored on. Under netting the
/// tracked trader always both sent and received, so no anchor could look like a pure sink. A
/// declared decode can anchor on an address the solver's own calldata named as the payee
/// (`KyberSwap`'s `dstReceiver`, paid straight from the pool), which sends nothing and would
/// otherwise be read as a fee wallet collecting the entire output — discarding exactly the
/// different-receiver trades the declared read exists to reach.
fn fee_on_transfer(
    flow: &SettledSwap,
    transfer_ledger: &TransferLedger,
    registry: &Registry,
    payee: Address,
) -> bool {
    let sides = [(flow.token_in, flow.amount_in), (flow.token_out, flow.amount_out)];
    for (token, trade_amount) in sides {
        for (recipient, total) in transfer_ledger.sink_payments(token) {
            if recipient == payee ||
                registry.is_fee_collector(recipient) ||
                registry.is_infrastructure(recipient)
            {
                continue;
            }
            if total.saturating_mul(U256::from(RESIDUE_GROSS_RATIO)) >= trade_amount {
                return true;
            }
        }
    }
    false
}

sol! {
    event TransferSingle(
        address indexed operator, address indexed from, address indexed to, uint256 id,
        uint256 value
    );
    event TransferBatch(
        address indexed operator, address indexed from, address indexed to, uint256[] ids,
        uint256[] values
    );
}

/// Whether `recipient` received an NFT (ERC-721 or ERC-1155) in the transaction.
///
/// An ERC-721 `Transfer` shares the ERC-20 event signature but indexes all three parameters
/// (four topics, empty data), so it is invisible to ERC-20 netting; ERC-1155 uses its own
/// events with the recipient as the third indexed parameter. A trader who received an NFT was
/// buying, not swapping: the netted token flow is the payment side of a purchase and the real
/// consideration is invisible, so recording it would pair the payment with the change.
fn received_nft(logs: &[Log], recipient: Address) -> bool {
    for log in logs {
        let topics = log.topics();
        let Some(&signature) = topics.first() else {
            continue;
        };
        let to = if signature == Transfer::SIGNATURE_HASH && topics.len() == 4 {
            topics[2]
        } else if (signature == TransferSingle::SIGNATURE_HASH ||
            signature == TransferBatch::SIGNATURE_HASH) &&
            topics.len() == 4
        {
            topics[3]
        } else {
            continue;
        };
        if Address::from_word(to) == recipient {
            return true;
        }
    }
    false
}

/// A wrap-pair trade (native <-> wrapped native) more than this factor off 1:1 is mis-paired:
/// wrapping is exactly 1:1 by construction, and venue fees only remove a few percent, so nothing
/// legitimate strays this far.
const WRAP_PAIR_MAX_RATIO: u64 = 2;

/// Whether a "swap" between the native token and its wrapped form has amounts a wrap or unwrap
/// cannot produce.
///
/// Seen with cross-chain deposits where the trader sends WETH and the only same-chain receipt is
/// a dust remainder refund in native ETH — netting pairs the two into a trade that never happened,
/// orders of magnitude off parity.
fn wrap_pair_mispaired(swap: &SettledSwap, wrapped_native: Address) -> bool {
    let pair = [swap.token_in, swap.token_out];
    if !(pair.contains(&Address::ZERO) && pair.contains(&wrapped_native)) {
        return false;
    }
    let max = U256::from(WRAP_PAIR_MAX_RATIO);
    swap.amount_in > swap.amount_out.saturating_mul(max) ||
        swap.amount_out > swap.amount_in.saturating_mul(max)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Log as PrimitiveLog;

    use super::*;
    use crate::decoder::test_utils::{addr, make_nft_transfer_log, make_transfer_log, swap};

    #[test]
    fn test_received_nft_erc721() {
        // The NFT purchase shape: buyer pays a token and receives an ERC-721, not a token amount.
        let buyer = addr(1);
        let seller = addr(2);
        let collection = addr(60);

        let logs = vec![make_nft_transfer_log(collection, seller, buyer, 4002)];
        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn test_received_nft_erc1155_single() {
        let buyer = addr(1);
        let operator = addr(3);
        let seller = addr(2);
        let collection = addr(60);

        let event = TransferSingle {
            operator,
            from: seller,
            to: buyer,
            id: U256::from(7),
            value: U256::from(1),
        };
        let data = event.encode_log_data();
        let primitive =
            PrimitiveLog::new_unchecked(collection, data.topics().to_vec(), data.data.clone());
        let logs = vec![Log { inner: primitive, ..Default::default() }];

        assert!(received_nft(&logs, buyer));
        assert!(!received_nft(&logs, seller));
    }

    #[test]
    fn test_received_nft_erc20_only() {
        // A plain ERC-20 Transfer (three topics, amount in data) must not read as an NFT even
        // though it shares the event signature.
        let user = addr(1);
        let logs = vec![make_transfer_log(addr(10), addr(2), user, U256::from(1000))];
        assert!(!received_nft(&logs, user));
    }

    #[test]
    fn test_wrap_pair_dust_refund() {
        // Relay cross-chain deposit shape (tx 0xc9de04eb…): 0.02 WETH in, a billionth of it
        // refunded back as native ETH — not an unwrap.
        let weth = addr(20);
        let deposit = swap(weth, 20_129_551_554_664_188, Address::ZERO, 1_554_664_188);
        assert!(wrap_pair_mispaired(&deposit, weth));

        let reversed = swap(Address::ZERO, 1_000_000, weth, 100);
        assert!(wrap_pair_mispaired(&reversed, weth));
    }

    #[test]
    fn test_wrap_pair_near_parity() {
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 1000), weth));
        // An unwrap with a fee taken stays within the 2x band.
        assert!(!wrap_pair_mispaired(&swap(weth, 1000, Address::ZERO, 900), weth));
    }

    fn flow(tracked: Address, net: SettledSwap) -> SettledSwap {
        SettledSwap { tracked, ..net }
    }

    #[test]
    fn test_fee_on_transfer_input_tax() {
        // The transfer-tax shape (ZRP on Polygon): the token contract splits the trader's own
        // transfer, 95% to the pool and 5% to a tax wallet that sends nothing, while netting
        // reads the full outflow as amount_in.
        let trader = addr(1);
        let pool = addr(50);
        let tax_wallet = addr(60);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool, U256::from(950)),
            make_transfer_log(token_in, trader, tax_wallet, U256::from(50)),
            make_transfer_log(token_out, pool, trader, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let taxed = flow(trader, swap(token_in, 1000, token_out, 2000));

        assert_eq!(
            check(&taxed, &transfer_ledger, &logs, &Registry::ethereum(), trader),
            Some(Veto::FeeOnTransfer)
        );
    }

    #[test]
    fn test_fee_on_transfer_output_tax() {
        // The buy-side mirror: the pool pays the gross output and the token contract splits it —
        // 95% to the trader, 5% to the fee wallet. The pool never received the output token, so
        // it is the token's source, unlike a router that forwards what it was paid.
        let trader = addr(1);
        let pool = addr(50);
        let tax_wallet = addr(60);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, trader, U256::from(1900)),
            make_transfer_log(token_out, pool, tax_wallet, U256::from(100)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let taxed = flow(trader, swap(token_in, 1000, token_out, 1900));

        assert_eq!(
            check(&taxed, &transfer_ledger, &logs, &Registry::ethereum(), trader),
            Some(Veto::FeeOnTransfer)
        );
    }

    #[test]
    fn test_input_short_of_declared_is_a_fee_token() {
        // EverRise (RISE), live tx 0x40235d0b…: the Universal Router calldata authorised 1.0 and
        // the trader's transfer delivered 0.95. Tycho quotes RISE as fee-free, so the comparison
        // would hand Fynd the token's 5% cut.
        let trader = addr(1);
        let pool = addr(50);
        let rise = addr(10);
        let logs =
            vec![make_transfer_log(rise, trader, pool, U256::from(950_000_000_000_000_000u64))];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let declared = SettledSwap {
            tracked: trader,
            ..swap(rise, 1_000_000_000_000_000_000, Address::ZERO, 6_238_916_653)
        };
        assert!(input_short_of_declared(&declared, &ledger));
    }

    #[test]
    fn test_input_matching_the_transfer_is_not_a_fee_token() {
        let trader = addr(1);
        let token = addr(10);
        let logs = vec![make_transfer_log(token, trader, addr(50), U256::from(1_000u64))];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let flow = SettledSwap { tracked: trader, ..swap(token, 1_000, addr(11), 2_000) };
        assert!(!input_short_of_declared(&flow, &ledger));
    }

    #[test]
    fn test_input_larger_than_declared_is_a_fee_taken_before_the_swap() {
        // ParaSwap's shape: the trader spends more than the calldata's amount_in because a fee
        // was taken before the solver frame. The calldata figure is the amount that reached the
        // pools, which is correct — not a taxed token.
        let trader = addr(1);
        let token = addr(10);
        let logs = vec![make_transfer_log(token, trader, addr(50), U256::from(1_010u64))];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let flow = SettledSwap { tracked: trader, ..swap(token, 1_000, addr(11), 2_000) };
        assert!(!input_short_of_declared(&flow, &ledger));
    }

    #[test]
    fn test_third_party_funded_input_is_not_judged() {
        // The trader sent none of the input token — a solver rebalance or a third-party funder.
        // Nothing here can tell whether the token taxes transfers, so it is not a veto.
        let trader = addr(1);
        let token = addr(10);
        let logs = vec![make_transfer_log(token, addr(99), addr(50), U256::from(1_000u64))];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let flow = SettledSwap { tracked: trader, ..swap(token, 1_000, addr(11), 2_000) };
        assert!(!input_short_of_declared(&flow, &ledger));
    }

    #[test]
    fn test_fee_on_transfer_with_a_declared_payee_anchor() {
        // A different-receiver trade, which only the declared read can reach: the pool pays the
        // whole output straight to the payee the calldata named (KyberSwap's `dstReceiver`), who
        // sends nothing. That payee looks exactly like a fee wallet collecting the entire output,
        // so without the exemption the trade is discarded as a token tax.
        let registry = Registry::ethereum();
        let trader = addr(1);
        let payee = addr(2);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, payee, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let delivered = flow(trader, swap(token_in, 1000, token_out, 2000));

        assert_eq!(check(&delivered, &transfer_ledger, &logs, &registry, payee), None);
        // Anchored on the trader instead, the same payee is an unexplained sink — which is what
        // the veto is for, and why the anchor has to be passed in rather than assumed.
        assert_eq!(
            check(&delivered, &transfer_ledger, &logs, &registry, trader),
            Some(Veto::FeeOnTransfer)
        );
    }

    #[test]
    fn test_fee_collector_sink_is_not_a_transfer_fee() {
        // The same split, but the sink is a registered venue fee collector: a venue fee the
        // decoders back out, not a token tax.
        let registry = Registry::ethereum();
        let collector = *registry
            .venue_fees()
            .keys()
            .next()
            .unwrap();
        let trader = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool, U256::from(950)),
            make_transfer_log(token_in, trader, collector, U256::from(50)),
            make_transfer_log(token_out, pool, trader, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let fee_paying = flow(trader, swap(token_in, 1000, token_out, 2000));

        assert_eq!(check(&fee_paying, &transfer_ledger, &logs, &registry, trader), None);
    }

    #[test]
    fn test_router_paid_fee_is_not_a_transfer_fee() {
        // The partner-fee shape (ParaSwap): the router pays a cut of the output to a fee wallet.
        // The sink leg comes from the router, not the trader, so it is a solver fee — part
        // of the settled price by policy, never a fee-on-transfer veto.
        let trader = addr(1);
        let pool = addr(50);
        let router = addr(2);
        let fee_wallet = addr(60);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, router, U256::from(2000)),
            make_transfer_log(token_out, router, fee_wallet, U256::from(30)),
            make_transfer_log(token_out, router, trader, U256::from(1970)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let partner_fee = flow(trader, swap(token_in, 1000, token_out, 1970));

        assert_eq!(
            check(&partner_fee, &transfer_ledger, &logs, &Registry::ethereum(), trader),
            None
        );
    }

    #[test]
    fn test_split_route_pools_are_not_sinks() {
        // A split route pays two pools; both send output onward, so neither is a sink.
        let trader = addr(1);
        let pool_a = addr(50);
        let pool_b = addr(51);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool_a, U256::from(600)),
            make_transfer_log(token_in, trader, pool_b, U256::from(400)),
            make_transfer_log(token_out, pool_a, trader, U256::from(1200)),
            make_transfer_log(token_out, pool_b, trader, U256::from(800)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let split = flow(trader, swap(token_in, 1000, token_out, 2000));

        assert_eq!(check(&split, &transfer_ledger, &logs, &Registry::ethereum(), trader), None);
    }

    #[test]
    fn test_dust_sink_leg_is_not_a_transfer_fee() {
        // A sink leg under the 1% residue line is rounding dust, not a tax.
        let trader = addr(1);
        let pool = addr(50);
        let sink = addr(60);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, trader, pool, U256::from(9950)),
            make_transfer_log(token_in, trader, sink, U256::from(50)),
            make_transfer_log(token_out, pool, trader, U256::from(20_000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);
        let dusty = flow(trader, swap(token_in, 10_000, token_out, 20_000));

        assert_eq!(check(&dusty, &transfer_ledger, &logs, &Registry::ethereum(), trader), None);
    }

    #[test]
    fn test_non_wrap_pair() {
        // Ordinary token pairs legitimately trade at any rate (decimals differ), and a
        // token <-> wrapped-native trade without the native side is a real swap too.
        let weth = addr(20);
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, addr(11), 5), weth));
        assert!(!wrap_pair_mispaired(&swap(addr(10), 1_000_000_000, weth, 5), weth));
        assert!(!wrap_pair_mispaired(&swap(Address::ZERO, 1_000_000_000, addr(11), 5), weth));
    }
}
