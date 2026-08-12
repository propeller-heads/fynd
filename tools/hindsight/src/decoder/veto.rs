//! Vetoes: rejections that keep non-trades out of the records, and the `Veto` type they all
//! share.
//!
//! There are two veto points. Solver-specific vetoes run at match time on logs alone (see
//! `solvers::solver_veto`), before a transaction costs a trace. The checks in this module run
//! after decoding: netting can pair value legs that were never a swap — the payment side of an
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
    decode::TraderFlow,
    registry::Registry,
    transfer_ledger::{NetSwap, Transfer, TransferLedger, RESIDUE_GROSS_RATIO},
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
    /// destination chain, so there is no same-chain swap to record. Placed at match time by
    /// `solvers::solver_veto`, never by `check`.
    BridgeOrder,
    /// Part of the trader's input never reached routing: the trader's own transfer of the input
    /// token split a significant share off to an address that only accumulates — a
    /// fee-on-transfer token's tax wallet, or an unregistered input-side fee. The skim nets into
    /// `amount_in`, and the re-solve cannot model it, so every comparison would credit Fynd with
    /// value no routing can recover.
    FeeOnTransfer,
}

/// Check a decoded flow against every post-decode veto, returning the first.
pub(crate) fn check(
    flow: &TraderFlow,
    transfer_ledger: &TransferLedger,
    logs: &[Log],
    registry: &Registry,
) -> Option<Veto> {
    if received_nft(logs, flow.tracked) {
        return Some(Veto::NftPurchase);
    }
    if wrap_pair_mispaired(&flow.swap, registry.wrapped_native()) {
        return Some(Veto::MispairedWrapPair);
    }
    if input_skimmed(flow, transfer_ledger, registry) {
        return Some(Veto::FeeOnTransfer);
    }
    None
}

/// Whether a significant share of the trader's input-token outflow went to a pure sink that is
/// not a registered fee collector.
///
/// A fee-on-transfer token's tax is invisible to netting: the token contract splits the trader's
/// own transfer, so the full outflow nets as `amount_in` while only the untaxed part reached the
/// route (ZRP on Polygon taxes 5%, and every settled trade read as a constant ~525 bps win). The
/// tax leg's fingerprint is that it comes straight from the trader and lands on an address that
/// sends nothing all transaction — a pool or router always sends something onward. Registered fee
/// collectors share the shape but carry venue fees, which the venue decoders and
/// `venue_attribution` back out — so they are exempt. A leg under the residue line (1% of the
/// input, `RESIDUE_GROSS_RATIO`) is dust, not a skim.
fn input_skimmed(flow: &TraderFlow, transfer_ledger: &TransferLedger, registry: &Registry) -> bool {
    for (recipient, amount) in transfer_ledger.sink_payments_from(flow.tracked, flow.swap.token_in)
    {
        if registry.is_fee_collector(recipient) || registry.is_infrastructure(recipient) {
            continue;
        }
        if amount.saturating_mul(U256::from(RESIDUE_GROSS_RATIO)) >= flow.swap.amount_in {
            return true;
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
fn wrap_pair_mispaired(swap: &NetSwap, wrapped_native: Address) -> bool {
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

    fn flow(tracked: Address, net: NetSwap) -> TraderFlow {
        TraderFlow::without_fees(tracked, net)
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
            check(&taxed, &transfer_ledger, &logs, &Registry::ethereum()),
            Some(Veto::FeeOnTransfer)
        );
    }

    #[test]
    fn test_fee_collector_sink_is_not_a_skim() {
        // The same split, but the sink is a registered venue fee collector: a venue fee the
        // decoders back out, not a token tax.
        let registry = Registry::ethereum();
        let collector = *registry
            .venue("metamask")
            .unwrap()
            .fee_collectors
            .iter()
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

        assert_eq!(check(&fee_paying, &transfer_ledger, &logs, &registry), None);
    }

    #[test]
    fn test_router_paid_fee_is_not_a_skim() {
        // The partner-fee shape (ParaSwap): the router skims the output to a fee wallet. The
        // sink leg comes from the router, not the trader, so it is a solver fee — part of the
        // settled price by policy, never a fee-on-transfer veto.
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

        assert_eq!(check(&partner_fee, &transfer_ledger, &logs, &Registry::ethereum()), None);
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

        assert_eq!(check(&split, &transfer_ledger, &logs, &Registry::ethereum()), None);
    }

    #[test]
    fn test_dust_sink_leg_is_not_a_skim() {
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

        assert_eq!(check(&dusty, &transfer_ledger, &logs, &Registry::ethereum()), None);
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
