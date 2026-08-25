//! Transfer-netting: recover a swap from what actually moved.
//!
//! The evidence is the ERC-20 `Transfer` events plus the native transfers recovered from the
//! trace (see `transfer_ledger`) — what actually moved, not what any contract or calldata
//! declared. It needs no knowledge of any router's format, which is also its weakness: a venue fee
//! taken out of the trade sits inside the netted amounts, since netting reads the trader's gross
//! spend and receipt. A fee paid to a wallet in `[venue_fees]` is corrected out by
//! `super::attribution::venue`; any other fee stays inside. Netted records are therefore the
//! marked fallback tier (`decode: "netted"`), excluded from the report by default, and the
//! declared decode (see `super::declared`) is the trusted path.
//!
//! Netting requires the trader to both pay and receive. When the swap's output is delivered to a
//! different receiver, nothing nets against the trader's input and the transaction is declined —
//! a coverage miss, never wrong amounts (see `transfer_ledger` for the model's assumptions).

use std::collections::HashMap;

use alloy::{primitives::Address, providers::Provider};
use tracing::warn;

use crate::decoder::{
    registry::Registry,
    transfer_ledger::{SettledSwap, TransferLedger},
};

/// Net the trade the declared decode could not read, picking whose balances count as the trade
/// from the entry point:
///
/// - a venue entry or a solver entry is a direct swap: the sender is the trader;
/// - a batch settlement or a log-matched intent fill is sent by a solver, so the trader is found in
///   the transfers instead.
///
/// Returns the decoder label recorded on the trade with the flow.
pub(crate) async fn fallback_flow<P: Provider>(
    provider: &P,
    code_cache: &mut HashMap<Address, bool>,
    registry: &Registry,
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<(&'static str, SettledSwap)> {
    if registry
        .venue_name(entry_point)
        .is_some()
    {
        return sender_flow(transfer_ledger, sender, entry_point)
            .map(|flow| ("venue-netting", flow));
    }
    if registry.is_solver(entry_point) && !registry.is_batch_settler(entry_point) {
        return sender_flow(transfer_ledger, sender, entry_point)
            .map(|flow| ("sender-netting", flow));
    }
    // Batch settlements and log-matched intent fills: the sender acts for the trader. A
    // frame-matched transaction through an unknown wrapper has no other trader to find, so it
    // falls through to the sender's own flow.
    if let Some(flow) =
        find_intent_trade(provider, transfer_ledger, &[entry_point, sender], registry, code_cache)
            .await
    {
        return Some(("intent-netting", flow));
    }
    sender_flow(transfer_ledger, sender, entry_point).map(|flow| ("sender-netting", flow))
}

/// Net the sender's flow. When the sender nets nothing, fall back to the contract the transaction
/// entered through (`tx.to`), for the rare shape where the swap output is delivered to that
/// contract rather than back to the sender.
pub(crate) fn sender_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<SettledSwap> {
    transfer_ledger
        .net_swap(sender)
        .or_else(|| transfer_ledger.net_swap(entry_point))
}

/// Find the order swapper's trade in a solver-initiated intent fill.
///
/// The transaction sender is the solver, not the swapper, so we look for the
/// externally-owned account whose net flow is a clean two-token swap. Contracts
/// never qualify (checked via `eth_getCode`): pools and routers net the inverse
/// swap or leftover dust, and recording an intermediary's dust as the trade
/// produces absurd "swaps" (seen live: WETH → 2.4e-7 AAVE). Known registry
/// contracts and the excluded addresses (solver, entry point) are skipped too.
/// A fill with no clean-net EOA is declined rather than guessed.
///
/// v0 limitations (tracked for a decode/attribution rework):
/// - **One swapper per transaction.** The first clean-net EOA wins, so a batch that settles several
///   retail orders in one tx contributes a single decoded trade; the rest surface as "Allium only"
///   gaps and batch volume is under-counted.
/// - **No settlement-tied tiebreak.** When several non-excluded EOAs each net to a clean two-token
///   swap, the winner is just the first in `intent_candidates`' address-ordered iteration, so a
///   decode can attribute the wrong account's flow.
/// - **Smart-wallet swappers are declined.** A swapper behind contract code (account abstraction,
///   EIP-7702 delegation) is indistinguishable from a pool here, so its fills are dropped.
pub(crate) async fn find_intent_trade<P: Provider>(
    provider: &P,
    transfer_ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
    code_cache: &mut HashMap<Address, bool>,
) -> Option<SettledSwap> {
    for flow in intent_candidates(transfer_ledger, exclude, registry) {
        if !is_contract(provider, flow.tracked, code_cache).await {
            return Some(flow);
        }
    }
    None
}

/// Addresses with a clean two-token net swap, excluding the zero address, the
/// excluded addresses, and known registry contracts. Ordered by address for
/// deterministic selection.
fn intent_candidates(
    transfer_ledger: &TransferLedger,
    exclude: &[Address],
    registry: &Registry,
) -> Vec<SettledSwap> {
    let mut candidates = transfer_ledger.participants();
    candidates.remove(&Address::ZERO);
    candidates.retain(|address| !exclude.contains(address) && !registry.is_known(*address));

    let mut flows = Vec::new();
    for candidate in candidates {
        if let Some(flow) = transfer_ledger.net_swap(candidate) {
            flows.push(flow);
        }
    }
    flows
}

/// Whether an address has contract code, cached across blocks. On RPC failure
/// the address is treated as a contract so it is not mistaken for an EOA
/// swapper.
///
/// v0 limitation: an EIP-7702-delegated account carries code, so a 7702 swapper EOA is classified
/// as a contract and dropped. 7702 is not yet widely used, so this is accepted for now.
async fn is_contract<P: Provider>(
    provider: &P,
    address: Address,
    cache: &mut HashMap<Address, bool>,
) -> bool {
    if let Some(is_contract) = cache.get(&address) {
        return *is_contract;
    }
    let is_contract = match provider.get_code_at(address).await {
        Ok(code) => !code.is_empty(),
        Err(error) => {
            warn!(%address, %error, "failed to fetch code; treating as contract");
            true
        }
    };
    cache.insert(address, is_contract);
    is_contract
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{Bytes, U256},
        providers::RootProvider,
        rpc::client::RpcClient,
        transports::mock::Asserter,
    };

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    fn mocked_provider(asserter: &Asserter) -> RootProvider {
        RootProvider::new(RpcClient::mocked(asserter.clone()))
    }

    fn relay_entry(registry: &Registry) -> Address {
        *registry
            .venue("relay")
            .unwrap()
            .entry_points
            .iter()
            .next()
            .unwrap()
    }

    /// The swapper/pool inverse-swap fixture: swapper sells `token_a` for `token_b`, pool nets the
    /// inverse.
    fn inverse_swap_ledger() -> TransferLedger {
        let logs = vec![
            make_transfer_log(addr(10), addr(100), addr(101), U256::from(1000)),
            make_transfer_log(addr(11), addr(101), addr(100), U256::from(2000)),
        ];
        TransferLedger::from_transaction(&logs, &[])
    }

    #[tokio::test]
    async fn test_fallback_venue_entry_nets_the_sender() {
        // User swap through a venue entry point: the sender's own net flow is the trade. A venue
        // fee taken from the input stays inside `amount_in` — the record is marked netted, and the
        // marker is what carries that inaccuracy.
        let registry = Registry::ethereum();
        let router = relay_entry(&registry);
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, addr(99), U256::from(40)),
            make_transfer_log(token_in, router, pool, U256::from(960)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let provider = mocked_provider(&Asserter::new());
        let mut cache = HashMap::new();

        let (decoder, flow) =
            fallback_flow(&provider, &mut cache, &registry, &ledger, user, router)
                .await
                .unwrap();
        assert_eq!(decoder, "venue-netting");
        assert_eq!(flow, SettledSwap { tracked: user, ..swap(token_in, 1000, token_out, 2000) });
    }

    #[tokio::test]
    async fn test_fallback_direct_solver_nets_the_sender() {
        let registry = Registry::ethereum();
        let oneinch: Address = "0x111111125421ca6dc452d289314280a0f8842a65"
            .parse()
            .unwrap();
        let user = addr(1);
        let pool = addr(50);
        let logs = vec![
            make_transfer_log(addr(10), user, pool, U256::from(1000)),
            make_transfer_log(addr(11), pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let provider = mocked_provider(&Asserter::new());
        let mut cache = HashMap::new();

        let (decoder, flow) =
            fallback_flow(&provider, &mut cache, &registry, &ledger, user, oneinch)
                .await
                .unwrap();
        assert_eq!(decoder, "sender-netting");
        assert_eq!(flow, SettledSwap { tracked: user, ..swap(addr(10), 1000, addr(11), 2000) });
    }

    #[tokio::test]
    async fn test_fallback_batch_settler_finds_the_swapper() {
        // A CoW batch the log decode declined (multi-order): the sender is the solver, so the
        // trader is the clean-net EOA in the transfers.
        let registry = Registry::ethereum();
        let cow: Address = "0x9008d19f58aabd9ed0d60971565aa8510560ab41"
            .parse()
            .unwrap();
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::default()); // the swapper is an EOA
        let provider = mocked_provider(&asserter);
        let mut cache = HashMap::new();

        let (decoder, flow) =
            fallback_flow(&provider, &mut cache, &registry, &inverse_swap_ledger(), addr(2), cow)
                .await
                .unwrap();
        assert_eq!(decoder, "intent-netting");
        assert_eq!(flow.tracked, addr(100));
    }

    #[tokio::test]
    async fn test_find_intent_trade_eoa_candidate() {
        let asserter = Asserter::new();
        // Candidates in address order: addr(100) first — an EOA (empty code).
        asserter.push_success(&Bytes::default());
        let provider = mocked_provider(&asserter);

        let registry = Registry::ethereum();
        let mut cache = HashMap::new();
        let flow = find_intent_trade(&provider, &inverse_swap_ledger(), &[], &registry, &mut cache)
            .await
            .unwrap();
        assert_eq!(
            flow,
            SettledSwap { tracked: addr(100), ..swap(addr(10), 1000, addr(11), 2000) }
        );
    }

    #[tokio::test]
    async fn test_find_intent_trade_all_candidates_contracts() {
        let asserter = Asserter::new();
        // Both candidates carry code: a routing intermediary and a pool. Guessing one would net
        // residue dust as an absurd swap, so the fill must be declined.
        asserter.push_success(&Bytes::from(vec![0xfe]));
        asserter.push_success(&Bytes::from(vec![0xfe]));
        let provider = mocked_provider(&asserter);

        let registry = Registry::ethereum();
        let mut cache = HashMap::new();
        let flow =
            find_intent_trade(&provider, &inverse_swap_ledger(), &[], &registry, &mut cache).await;
        assert!(flow.is_none());
    }

    #[test]
    fn test_intent_candidates_swap_sides() {
        // Intent fill: the swapper sells token_a for token_b; the pool is the
        // counterparty. The solver is excluded.
        let registry = Registry::ethereum();
        let swapper = addr(100);
        let pool = addr(101);
        let solver = addr(102);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, swapper, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, swapper, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        let found: HashMap<Address, _> = intent_candidates(&transfer_ledger, &[solver], &registry)
            .into_iter()
            .map(|flow| (flow.tracked, flow))
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[&swapper],
            SettledSwap { tracked: swapper, ..swap(token_a, 1000, token_b, 2000) }
        );
        // The pool nets the inverse swap; the EOA filter discards it later.
        assert_eq!(
            found[&pool],
            SettledSwap { tracked: pool, ..swap(token_b, 2000, token_a, 1000) }
        );
    }

    #[test]
    fn test_intent_candidates_excluded_and_known() {
        let registry = Registry::ethereum();
        let swapper = addr(100);
        let pool = addr(101);
        let token_a = addr(10);
        let token_b = addr(11);

        let logs = vec![
            make_transfer_log(token_a, swapper, pool, U256::from(1000)),
            make_transfer_log(token_b, pool, swapper, U256::from(2000)),
        ];
        let transfer_ledger = TransferLedger::from_transaction(&logs, &[]);

        // Excluding the swapper leaves only the pool.
        let candidates = intent_candidates(&transfer_ledger, &[swapper], &registry);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].tracked, pool);
    }
}
