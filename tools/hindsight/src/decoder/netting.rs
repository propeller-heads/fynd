//! Transfer-netting: recover a swap from what actually moved.
//!
//! The evidence is the ERC-20 `Transfer` events plus the native transfers recovered from the
//! trace (see `transfer_ledger`) — what actually moved, not what any contract or calldata
//! declared. It needs no knowledge of any router's format, which is also its weakness: a fee the
//! ledger does not show (or whose collector is not in the address book) sits inside the netted
//! amounts. Netted records are therefore the marked fallback tier (`decode: "netted"`), excluded
//! from the report by default; the declared decode (see `super::declared`) is the trusted path.
//!
//! Netting requires the trader to both pay and receive. When the swap's output is delivered to a
//! different receiver, nothing nets against the trader's input and the transaction is declined —
//! a coverage miss, never wrong amounts (see `transfer_ledger` for the model's assumptions).

use std::collections::{HashMap, HashSet};

use alloy::{primitives::Address, providers::Provider};
use tracing::warn;

use crate::decoder::{
    registry::Registry,
    transfer_ledger::{NetSwap, TransferLedger},
};

/// The trader's side of a matched transaction: the swap, plus the venue fees that make it
/// comparable.
pub(crate) struct TraderFlow {
    /// The address whose flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Venue fee taken from the input token. On a netted flow it is already backed out of
    /// `swap.amount_in`; on a declared flow it is recorded only (the declared amount is already
    /// post-fee).
    pub venue_fee_in: Option<alloy::primitives::U256>,
    /// Venue fee taken from the output token. On a netted flow it is already added back into
    /// `swap.amount_out`; on a declared flow it is recorded only.
    pub venue_fee_out: Option<alloy::primitives::U256>,
}

impl TraderFlow {
    pub(crate) fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self { tracked, swap, venue_fee_in: None, venue_fee_out: None }
    }

    /// Record `fee` as an output-token venue fee and gross it back into `swap.amount_out`, so the
    /// settled output stays comparable to Fynd's gross re-solve. A no-op when an output fee was
    /// already accounted, so a second matching fee leg cannot double-count.
    pub(crate) fn gross_output_fee(&mut self, fee: alloy::primitives::U256) {
        if self.venue_fee_out.is_some() {
            return;
        }
        self.venue_fee_out = Some(fee);
        self.swap.amount_out = self.swap.amount_out.saturating_add(fee);
    }

    /// Record `fee` as an input-token venue fee and net it out of `swap.amount_in`, so the settled
    /// input is what actually reached the pools rather than the user's gross spend. A no-op when an
    /// input fee was already accounted.
    ///
    /// Without this, a venue skimming its fee off the input makes the settled trade look bigger
    /// than it was, and Fynd — re-solved on that inflated size — appears to beat it.
    pub(crate) fn net_input_fee(&mut self, fee: alloy::primitives::U256) {
        if self.venue_fee_in.is_some() {
            return;
        }
        self.venue_fee_in = Some(fee);
        self.swap.amount_in = self.swap.amount_in.saturating_sub(fee);
    }
}

/// Net the trade the declared decode could not read, picking whose balances count as the trade
/// from the entry point:
///
/// - a venue entry nets the sender and backs the venue's fee out (collectors from the address
///   book);
/// - a batch settlement or a log-matched intent fill is sent by a solver, so the trader is found in
///   the transfers instead;
/// - a solver entry is a direct swap: the sender is the trader.
///
/// Returns the decoder label recorded on the trade with the flow.
pub(crate) async fn fallback_flow<P: Provider>(
    provider: &P,
    code_cache: &mut HashMap<Address, bool>,
    registry: &Registry,
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
) -> Option<(&'static str, TraderFlow)> {
    if let Some(venue) = registry
        .venue_name(entry_point)
        .and_then(|name| registry.venue(name))
    {
        return venue_flow(transfer_ledger, sender, entry_point, &venue.fee_collectors)
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
) -> Option<TraderFlow> {
    transfer_ledger
        .net_swap(sender)
        .map(|swap| TraderFlow::without_fees(sender, swap))
        .or_else(|| {
            transfer_ledger
                .net_swap(entry_point)
                .map(|swap| TraderFlow::without_fees(entry_point, swap))
        })
}

/// Net the sender's flow and back the venue's fee out of it — the shared shape of every
/// fee-taking venue entry.
///
/// One exception to the fee back-out: when the tracked trader IS a fee collector, the transaction
/// is a treasury operation — the collector's receipts are its own output, not a fee, and backing
/// them "out" would add the output to itself and double it.
pub(crate) fn venue_flow(
    transfer_ledger: &TransferLedger,
    sender: Address,
    entry_point: Address,
    fee_collectors: &HashSet<Address>,
) -> Option<TraderFlow> {
    let flow = sender_flow(transfer_ledger, sender, entry_point)?;
    if fee_collectors.contains(&flow.tracked) {
        return Some(flow);
    }
    Some(back_out_venue_fees(flow, transfer_ledger, fee_collectors))
}

/// Back a venue fee out of a decoded user flow.
///
/// The venue can take its fee on either side. An input-side fee is subtracted from `amount_in`
/// (the user's gross spend included money that never entered the swap) and an output-side fee is
/// added back into `amount_out` (the swap produced more than the user kept), so both sides are the
/// amounts actually swapped — the like-for-like basis vs Fynd.
fn back_out_venue_fees(
    mut flow: TraderFlow,
    transfer_ledger: &TransferLedger,
    fee_collectors: &HashSet<Address>,
) -> TraderFlow {
    let fees = transfer_ledger.received_by(fee_collectors);
    if let Some(fee) = fees
        .get(&flow.swap.token_in)
        .copied()
        .filter(|fee| !fee.is_zero())
    {
        flow.net_input_fee(fee);
    }
    if let Some(fee) = fees
        .get(&flow.swap.token_out)
        .copied()
        .filter(|fee| !fee.is_zero())
    {
        flow.gross_output_fee(fee);
    }
    flow
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
) -> Option<TraderFlow> {
    for (candidate, trade) in intent_candidates(transfer_ledger, exclude, registry) {
        if !is_contract(provider, candidate, code_cache).await {
            return Some(TraderFlow::without_fees(candidate, trade));
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
) -> Vec<(Address, NetSwap)> {
    let mut candidates = transfer_ledger.participants();
    candidates.remove(&Address::ZERO);
    candidates.retain(|address| !exclude.contains(address) && !registry.is_known(*address));

    let mut swaps = Vec::new();
    for candidate in candidates {
        if let Some(trade) = transfer_ledger.net_swap(candidate) {
            swaps.push((candidate, trade));
        }
    }
    swaps
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

    fn relay_collector(registry: &Registry) -> Address {
        *registry
            .venue("relay")
            .unwrap()
            .fee_collectors
            .iter()
            .next()
            .unwrap()
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
    async fn test_fallback_venue_entry_backs_the_fee_out() {
        // User swap through a venue entry point: sender nets token_in -> token_out, with an
        // input-side fee to the venue's collector (from the address book). The fee is backed out
        // of amount_in.
        let registry = Registry::ethereum();
        let collector = relay_collector(&registry);
        let router = relay_entry(&registry);
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);

        let logs = vec![
            make_transfer_log(token_in, user, router, U256::from(1000)),
            make_transfer_log(token_in, router, collector, U256::from(40)),
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
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(token_in, 960, token_out, 2000));
        assert_eq!(flow.venue_fee_in, Some(U256::from(40)));
        assert_eq!(flow.venue_fee_out, None);
    }

    #[tokio::test]
    async fn test_fallback_collector_is_the_trader() {
        // Treasury op: the fee collector itself unwraps WETH via the venue router. Its 1:1 native
        // receipt must not be treated as a fee and added back — that doubled the output.
        let registry = Registry::ethereum();
        let collector = relay_collector(&registry);
        let router = relay_entry(&registry);
        let weth = addr(10);

        let logs = vec![make_transfer_log(weth, collector, router, U256::from(1000))];
        let native = vec![(router, collector, U256::from(1000))];
        let ledger = TransferLedger::from_transaction(&logs, &native);
        let provider = mocked_provider(&Asserter::new());
        let mut cache = HashMap::new();

        let (_, flow) = fallback_flow(&provider, &mut cache, &registry, &ledger, collector, router)
            .await
            .unwrap();
        assert_eq!(flow.tracked, collector);
        assert_eq!(flow.swap, swap(weth, 1000, Address::ZERO, 1000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.venue_fee_out, None);
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
        assert_eq!(flow.tracked, user);
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
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
        assert_eq!(flow.tracked, addr(100));
        assert_eq!(flow.swap, swap(addr(10), 1000, addr(11), 2000));
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
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[&swapper], swap(token_a, 1000, token_b, 2000));
        // The pool nets the inverse swap; the EOA filter discards it later.
        assert_eq!(found[&pool], swap(token_b, 2000, token_a, 1000));
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
        assert_eq!(candidates[0].0, pool);
    }
}
