//! Relay decoding.
//!
//! Relay differs from direct solver swaps in two ways: its router sends a venue fee to a collector
//! address on either side of the swap, and its solvers submit rebalancing fills whose transaction
//! sender has no net flow.
//!
//! Two decoders, tried in order (`venues::decoders_for`): [`RelayCalldata`] reads the trader's
//! terms straight from the settling solver's own calldata, and [`RelayNetting`] nets the ledger
//! for the solvers `RelayCalldata` cannot parse (0x Settler) or transactions with no solver frame
//! at all. See `.claude/plans/calldata-first-decoding.md` for the empirics behind the ordering.

use std::collections::HashSet;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;

use crate::decoder::{
    decode::{DecodeContext, TradeDecoder, TraderFlow},
    netting::venue_flow,
    registry::VenueAddresses,
    solvers,
    transfer_ledger::{NetSwap, TransferLedger},
};

/// Relay's decoders, in try order, constructed with its address-book section (see
/// `venues::DECODERS`).
pub(crate) fn decoders(addresses: &VenueAddresses) -> Vec<Box<dyn TradeDecoder>> {
    vec![
        Box::new(RelayCalldata { addresses: addresses.clone() }),
        Box::new(RelayNetting { addresses: addresses.clone() }),
    ]
}

/// Relay's calldata-primary decoder.
///
/// Reads `token_in`/`token_out`/`amount_in` straight from the settling solver frame's `SwapIntent`
/// (already the post-fee, on-chain-enforced terms — Relay pays its input-side fee to the
/// collector *before* forwarding into the solver call) and recovers the settled `amount_out` as
/// the gross amount of `intent.token_out` received by the output recipient the same calldata
/// declares — the one field calldata can never carry. Declines (falling through to
/// [`RelayNetting`]) when no solver frame or intent is found, the recipient never received the
/// token, or either guard below fails.
///
/// Two guards protect against the recipient-receipt query mis-attributing a multi-order
/// transaction's output (see the design doc's risk section — not observed in the sampled traffic,
/// but cheap to check): the recovered output must clear the intent's on-chain floor (a successful
/// trade cleared it by construction, so a violation means the wrong legs were picked up), and,
/// when the calldata also declares a quote, it must sit within `plausible_quote`'s band of the
/// recovered output.
pub(crate) struct RelayCalldata {
    addresses: VenueAddresses,
}

#[async_trait]
impl TradeDecoder for RelayCalldata {
    fn name(&self) -> &'static str {
        "relay-calldata"
    }

    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        let settled = solvers::settled_intent(ctx.root, ctx.registry)?;
        let intent = settled.intent;

        let amount_out = ctx
            .transfer_ledger
            .received_by_address(settled.output_recipient, intent.token_out);
        if amount_out.is_zero() || amount_out < intent.min_amount_out {
            return None;
        }
        if let Some(quoted) = intent.declared_quote() {
            if !solvers::plausible_quote(quoted, amount_out) {
                return None;
            }
        }

        // Both fees are already on the right basis (§1 of the design doc): the intent's
        // `amount_in` is post-input-fee and the recipient's receipt is pre-output-fee, so neither
        // amount above needs adjusting — the fee is recorded for transparency only.
        let fees = ctx
            .transfer_ledger
            .received_by(&self.addresses.fee_collectors);
        let venue_fee_in = fees
            .get(&intent.token_in)
            .copied()
            .filter(|fee| !fee.is_zero());
        let venue_fee_out = fees
            .get(&intent.token_out)
            .copied()
            .filter(|fee| !fee.is_zero());

        Some(TraderFlow {
            tracked: ctx.receipt.from,
            swap: NetSwap {
                token_in: intent.token_in,
                amount_in: intent.amount_in,
                token_out: intent.token_out,
                amount_out,
            },
            venue_fee_in,
            venue_fee_out,
            solver_override: None,
        })
    }
}

/// Relay's netting decoder.
pub(crate) struct RelayNetting {
    addresses: VenueAddresses,
}

#[async_trait]
impl TradeDecoder for RelayNetting {
    fn name(&self) -> &'static str {
        "relay-netting"
    }

    /// The common case is a user swap: net the sender's flow, then back the venue fee out of it.
    /// When the sender has no net flow the transaction is a solver-initiated rebalancing fill,
    /// decoded by anchoring on the fee collector instead (Relay funds the swap from it); the
    /// collector is the funding source there, not a fee recipient, so no fee is backed out.
    async fn decode(&self, ctx: &mut DecodeContext<'_>) -> Option<TraderFlow> {
        if let Some(flow) = venue_flow(
            ctx.transfer_ledger,
            ctx.receipt.from,
            ctx.entry_point,
            &self.addresses.fee_collectors,
        ) {
            return Some(flow);
        }
        decode_rebalance(
            ctx.transfer_ledger,
            &self.addresses.fee_collectors,
            &self.addresses.entry_points,
            ctx.registry.wrapped_native(),
        )
        .map(|swap| TraderFlow::without_fees(ctx.receipt.from, swap))
    }
}

/// Decode a Relay solver-initiated rebalancing fill, where `tx.from` is a rotating solver EOA with
/// no net flow (so sender netting finds nothing) and the swap moves Relay's own liquidity.
///
/// Anchors on the fee collector, which always funds the input: `token_in` is the single token it
/// net-sends. The output is one of two shapes — the token that comes back to the collector (an
/// internal inventory rebalance), or the asset received by the single external recipient that
/// only receives and never sends (a cross-chain order fill; see
/// `TransferLedger::sink_receipts`).
///
/// Declines (returns `None`) when the shape is ambiguous: not exactly one input token, a
/// same-token "swap", more than one token back to the collector, or more than one external
/// recipient or output (a batched multi-order fill, like netting's multi-leg decline).
fn decode_rebalance(
    transfer_ledger: &TransferLedger,
    fee_collectors: &HashSet<Address>,
    relay_entry_points: &HashSet<Address>,
    wrapped_native: Address,
) -> Option<NetSwap> {
    let net_in: Vec<(Address, U256)> = transfer_ledger
        .group_net_sent(fee_collectors)
        .into_iter()
        .collect();
    if net_in.len() != 1 {
        return None;
    }
    let (token_in, amount_in) = net_in[0];

    // C2 internal rebalance: the collector net-receives exactly one (different) token.
    let net_recv: Vec<(Address, U256)> = transfer_ledger
        .group_net_received(fee_collectors)
        .into_iter()
        .collect();
    if !net_recv.is_empty() {
        if net_recv.len() != 1 || net_recv[0].0 == token_in {
            return None;
        }
        let (token_out, amount_out) = net_recv[0];
        return Some(NetSwap { token_in, amount_in, token_out, amount_out });
    }

    // C1 external fill: the single pure-sink recipient, excluding infrastructure (routers,
    // collector, the wrapped-native token, the zero address) and the input token.
    let mut outputs: Vec<(Address, U256)> = Vec::new();
    for (recipient, token, amount) in transfer_ledger.sink_receipts() {
        if relay_entry_points.contains(&recipient) ||
            fee_collectors.contains(&recipient) ||
            recipient == Address::ZERO ||
            recipient == wrapped_native
        {
            continue;
        }
        // A payout, not a swap: the collector's token reached an external recipient unconverted
        // (a cross-chain order settled from same-token inventory). There is no conversion to
        // re-solve — pairing the leftover (e.g. a gas top-up) as "the output" fabricated
        // seven-figure-bps wins. A genuine fill routes token_in into a pool, which sends
        // something back and is therefore never a pure sink.
        if token == token_in {
            return None;
        }
        outputs.push((token, amount));
    }
    if outputs.len() != 1 {
        return None;
    }
    let (token_out, amount_out) = outputs[0];
    Some(NetSwap { token_in, amount_in, token_out, amount_out })
}

#[cfg(test)]
mod tests {
    use alloy::rpc::types::Log;

    use super::*;
    use crate::decoder::{
        decode::{gas_scope, GasScope, TraderRole},
        registry::Registry,
        test_utils::{addr, frame, make_transfer_log, swap, venue_addresses, CtxFixture},
    };

    fn transfer_ledger(logs: &[Log], native: &[(Address, Address, U256)]) -> TransferLedger {
        TransferLedger::from_transaction(logs, native)
    }

    fn relay_collector(registry: &Registry) -> Address {
        *registry
            .venue("relay")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
    }

    /// A registered relay entry point, so `TraderRole::classify` resolves the Venue role.
    fn relay_entry_point(registry: &Registry) -> Address {
        *venue_addresses(registry, "relay")
            .entry_points
            .iter()
            .next()
            .unwrap()
    }

    /// The `RelayNetting` decoder constructed with the registry's relay addresses.
    fn relay_netting(registry: &Registry) -> RelayNetting {
        RelayNetting { addresses: venue_addresses(registry, "relay") }
    }

    /// Decode a Relay transaction through the full `RelayNetting` decoder.
    async fn decode(
        registry: &Registry,
        ledger: &TransferLedger,
        sender: Address,
        entry_point: Address,
    ) -> Option<TraderFlow> {
        let mut fixture = CtxFixture::new(sender, entry_point);
        let mut ctx = fixture.ctx(registry, ledger, &[]);
        relay_netting(registry)
            .decode(&mut ctx)
            .await
    }

    #[test]
    fn test_rebalance_external_token_fill() {
        let fee = addr(99);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let token_out = addr(11);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![
            make_transfer_log(token_in, fee, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, recipient, U256::from(2000)),
        ];
        let got = decode_rebalance(&transfer_ledger(&logs, &[]), &collectors, &routers, addr(200))
            .unwrap();
        assert_eq!(got, swap(token_in, 1000, token_out, 2000));
    }

    #[test]
    fn test_rebalance_external_native_eth_out() {
        let fee = addr(99);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![make_transfer_log(token_in, fee, pool, U256::from(1000))];
        let native = vec![(pool, recipient, U256::from(2000))];
        let got =
            decode_rebalance(&transfer_ledger(&logs, &native), &collectors, &routers, addr(200))
                .unwrap();
        assert_eq!(got, swap(token_in, 1000, Address::ZERO, 2000));
    }

    #[test]
    fn test_rebalance_internal_back_to_collector() {
        let fee = addr(99);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![
            make_transfer_log(token_in, fee, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, fee, U256::from(1001)),
        ];
        let got = decode_rebalance(&transfer_ledger(&logs, &[]), &collectors, &routers, addr(200))
            .unwrap();
        assert_eq!(got, swap(token_in, 1000, token_out, 1001));
    }

    #[test]
    fn test_rebalance_multi_recipient() {
        let fee = addr(99);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([addr(2)]);
        let logs = vec![
            make_transfer_log(token_in, fee, pool, U256::from(2000)),
            make_transfer_log(token_out, pool, addr(7), U256::from(1000)),
            make_transfer_log(token_out, pool, addr(8), U256::from(1000)),
        ];
        assert!(decode_rebalance(&transfer_ledger(&logs, &[]), &collectors, &routers, addr(200))
            .is_none());
    }

    #[test]
    fn test_rebalance_unconverted_payout() {
        // Live tx 0x455f5202…: the collector pays out its token unconverted to an external
        // recipient (cross-chain order settled from same-token inventory) plus a tiny native gas
        // top-up. Pairing the top-up as "the output" fabricated a 10-million-bps win — a payout
        // has no conversion to re-solve and must decline.
        let fee = addr(99);
        let router = addr(2);
        let recipient = addr(7);
        let gas_recipient = addr(8);
        let token_in = addr(10);
        let collectors = HashSet::from([fee]);
        let routers = HashSet::from([router]);
        let logs = vec![
            make_transfer_log(token_in, fee, router, U256::from(2_002_781_016u64)),
            make_transfer_log(token_in, router, recipient, U256::from(2_002_781_016u64)),
        ];
        let native = vec![(router, gas_recipient, U256::from(1_139_527_584_556_489u64))];
        assert!(decode_rebalance(
            &transfer_ledger(&logs, &native),
            &collectors,
            &routers,
            addr(200)
        )
        .is_none());
    }

    #[test]
    fn test_rebalance_without_collector_outflow() {
        let logs = vec![make_transfer_log(addr(10), addr(1), addr(50), U256::from(1000))];
        let collectors = HashSet::from([addr(99)]);
        let routers = HashSet::from([addr(2)]);
        assert!(decode_rebalance(&transfer_ledger(&logs, &[]), &collectors, &routers, addr(200))
            .is_none());
    }

    #[tokio::test]
    async fn test_user_flow_with_fee() {
        // User swap through Relay: sender nets token_in -> token_out, with an input-side fee to
        // the real Relay collector. The fee is backed out of amount_in.
        let registry = Registry::ethereum();
        let collector = relay_collector(&registry);
        let user = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = transfer_ledger(&logs, &[]);
        let flow = decode(&registry, &ledger, user, router)
            .await
            .unwrap();
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 960, token_out, 2000));
        assert_eq!(flow.venue_fee_in, Some(U256::from(40)));
        assert_eq!(flow.venue_fee_out, None);
        // Trader-funded venue entry: the derived scope charges the solver frame's gas.
        let role = TraderRole::classify(relay_entry_point(&registry), &registry);
        assert_eq!(gas_scope(role, &flow, &ledger, user), GasScope::SolverFrame);
    }

    #[tokio::test]
    async fn test_collector_is_the_trader() {
        // Treasury op (live tx 0x80a4c0…): the fee collector itself unwraps WETH via the router.
        // Its 1:1 native receipt must not be treated as a fee and added back — that doubled the
        // output.
        let registry = Registry::ethereum();
        let collector = relay_collector(&registry);
        let router = addr(2);
        let weth = addr(10);

        let logs = vec![make_transfer_log(weth, collector, router, U256::from(1000))];
        let native = vec![(router, collector, U256::from(1000))];

        let flow = decode(&registry, &transfer_ledger(&logs, &native), collector, router)
            .await
            .unwrap();
        assert_eq!(flow.tracked, collector);
        assert_eq!(flow.swap, swap(weth, 1000, Address::ZERO, 1000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
    }

    #[tokio::test]
    async fn test_rebalance_fill() {
        // Solver fill: the sender has no net flow; the collector funds the swap. No fee back-out.
        let registry = Registry::ethereum();
        let collector = relay_collector(&registry);
        let solver = addr(1);
        let router = addr(2);
        let pool = addr(50);
        let recipient = addr(7);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, collector, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, recipient, U256::from(2000)),
        ];

        let ledger = transfer_ledger(&logs, &[]);
        let flow = decode(&registry, &ledger, solver, router)
            .await
            .unwrap();
        assert_eq!(flow.tracked, solver);
        assert_eq!(flow.swap, swap(token_in, 1000, token_out, 2000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
        // The sender never funded the swap, so the derived scope charges nothing.
        let role = TraderRole::classify(relay_entry_point(&registry), &registry);
        assert_eq!(gas_scope(role, &flow, &ledger, solver), GasScope::NotCharged);
    }

    mod relay_calldata {
        use alloy::{primitives::address, rpc::types::trace::geth::CallFrame};

        use super::*;

        /// Fly's own router — same address on every chain (`docs.fly.trade`).
        const FLY: Address = address!("0x20f6ee51340adeed01a59b0e65cb3703f3dc860c");
        /// 0x's `AllowanceHolder` — a registered solver with no `swap_intent` support.
        const ZEROX: Address = address!("0xdef1c0ded9bec7f1a1670819833240f027b25eff");
        /// Relay's own router — in the live fixture this is both the entry point and the
        /// declared output recipient Fly's calldata carries (Relay receives and forwards).
        const ROUTER: Address = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");

        /// The real Fly calldata used by `solvers::fly`'s fixture tests: USDT in, native out,
        /// `amount_in` 19,694,643, `min_amount_out` 10,217,898,321,149,381, declared quote
        /// 10,321,109,415,302,405.
        fn fly_input() -> Vec<u8> {
            let text = include_str!("../solvers/fixtures/fly_input.txt").trim();
            alloy::hex::decode(text.strip_prefix("0x").unwrap_or(text)).unwrap()
        }

        const TOKEN_IN: Address = address!("0xfde4c96c8593536e31f229ea8f37b2ada2699bb2");
        const AMOUNT_IN: u64 = 19_694_643;
        const MIN_AMOUNT_OUT: u128 = 10_217_898_321_149_381;
        const QUOTED_AMOUNT_OUT: u128 = 10_321_109_415_302_405;

        /// A root frame: `sender -> router -> solver`, the solver frame carrying `input`.
        fn root_with_solver_frame(sender: Address, router: Address, solver: Address) -> CallFrame {
            let mut solver_call = frame("CALL", router, solver, 0);
            solver_call.input = fly_input().into();
            let mut root = frame("CALL", sender, router, 0);
            root.calls = vec![solver_call];
            root
        }

        async fn decode_calldata(
            registry: &Registry,
            root: &CallFrame,
            ledger: &TransferLedger,
            sender: Address,
            router: Address,
        ) -> Option<TraderFlow> {
            let decoder = RelayCalldata { addresses: venue_addresses(registry, "relay") };
            let mut fixture = CtxFixture::new(sender, router);
            fixture.set_root(root.clone());
            let mut ctx = fixture.ctx(registry, ledger, &[]);
            decoder.decode(&mut ctx).await
        }

        #[tokio::test]
        async fn test_decode_recovers_output_from_recipient_receipt() {
            // The router — the declared recipient — receives native ETH above the floor; the
            // sender pays the input token directly (sender-funded).
            let registry = Registry::ethereum();
            let sender = addr(1);
            let root = root_with_solver_frame(sender, ROUTER, FLY);
            let logs = vec![make_transfer_log(TOKEN_IN, sender, ROUTER, U256::from(AMOUNT_IN))];
            let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
            let ledger = TransferLedger::from_transaction(&logs, &native);

            let flow = decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .unwrap();
            assert_eq!(flow.tracked, sender);
            assert_eq!(flow.swap.token_in, TOKEN_IN);
            assert_eq!(flow.swap.token_out, Address::ZERO);
            assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
            assert_eq!(flow.swap.amount_out, U256::from(MIN_AMOUNT_OUT + 1_000));
        }

        #[tokio::test]
        async fn test_decode_below_floor_declines() {
            // The recipient's receipt sits under the intent's on-chain floor: a successful trade
            // clears its floor by construction, so this means the query mis-attributed.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let root = root_with_solver_frame(sender, ROUTER, FLY);
            let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT - 1))];
            let ledger = TransferLedger::from_transaction(&[], &native);

            assert!(decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .is_none());
        }

        #[tokio::test]
        async fn test_decode_no_recipient_receipt_declines() {
            let registry = Registry::ethereum();
            let sender = addr(1);
            let root = root_with_solver_frame(sender, ROUTER, FLY);
            let ledger = TransferLedger::from_transaction(&[], &[]);

            assert!(decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .is_none());
        }

        #[tokio::test]
        async fn test_decode_no_solver_frame_declines() {
            let registry = Registry::ethereum();
            let sender = addr(1);
            let root = frame("CALL", sender, ROUTER, 0);
            let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
            let ledger = TransferLedger::from_transaction(&[], &native);

            assert!(decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .is_none());
        }

        #[tokio::test]
        async fn test_decode_solver_without_intent_support_declines() {
            // 0x is a registered solver (matches `find_solver_frame`) but has no `swap_intent`
            // implementation: the calldata path has nothing to recover, so it falls through.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let root = root_with_solver_frame(sender, ROUTER, ZEROX);
            let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
            let ledger = TransferLedger::from_transaction(&[], &native);

            assert!(decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .is_none());
        }

        #[tokio::test]
        async fn test_decode_implausible_quote_declines() {
            // A recovered output more than 2x the declared quote: `plausible_quote`'s band would
            // reject it as a unit mismatch or a mis-attributed receipt, even though it clears the
            // floor comfortably.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let root = root_with_solver_frame(sender, ROUTER, FLY);
            let implausible = U256::from(QUOTED_AMOUNT_OUT) * U256::from(3u64);
            let native = vec![(addr(50), ROUTER, implausible)];
            let ledger = TransferLedger::from_transaction(&[], &native);

            assert!(decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .is_none());
        }

        #[tokio::test]
        async fn test_decode_collector_funded_is_not_charged_gas() {
            // The fee collector, not the sender, net-sends the input token: a solver-initiated
            // rebalance, which charges no gas to any trader.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let collector = relay_collector(&registry);
            let root = root_with_solver_frame(sender, ROUTER, FLY);
            let logs = vec![make_transfer_log(TOKEN_IN, collector, ROUTER, U256::from(AMOUNT_IN))];
            let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
            let ledger = TransferLedger::from_transaction(&logs, &native);

            let flow = decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .unwrap();
            let role = TraderRole::classify(ROUTER, &registry);
            assert_eq!(gas_scope(role, &flow, &ledger, sender), GasScope::NotCharged);
        }

        #[tokio::test]
        async fn test_decode_records_venue_fee_without_adjusting_amounts() {
            // An input-side fee leg to the real Relay collector: recorded for transparency, but
            // `amount_in` stays the intent's raw figure — it is already post-fee (§1 of the design
            // doc), unlike netting's fee back-out.
            let registry = Registry::ethereum();
            let sender = addr(1);
            let collector = relay_collector(&registry);
            let root = root_with_solver_frame(sender, ROUTER, FLY);
            let logs = vec![
                make_transfer_log(TOKEN_IN, sender, ROUTER, U256::from(AMOUNT_IN)),
                make_transfer_log(TOKEN_IN, ROUTER, collector, U256::from(40)),
            ];
            let native = vec![(addr(50), ROUTER, U256::from(MIN_AMOUNT_OUT + 1_000))];
            let ledger = TransferLedger::from_transaction(&logs, &native);

            let flow = decode_calldata(&registry, &root, &ledger, sender, ROUTER)
                .await
                .unwrap();
            assert_eq!(flow.swap.amount_in, U256::from(AMOUNT_IN));
            assert_eq!(flow.venue_fee_in, Some(U256::from(40)));
        }
    }
}
