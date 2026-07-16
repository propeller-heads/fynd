//! Decode strategies: the methods for recovering a trader's swap from a matched transaction.
//!
//! A strategy is a complete method for answering one question: given one matched, traced
//! transaction, what swap did the trader perform? What distinguishes one strategy from another
//! is the on-chain evidence it reads:
//!
//! - **Value movements** — ERC-20 `Transfer` events plus native transfers recovered from the trace:
//!   what actually moved.
//! - **Protocol event logs** — a venue's or solver's own emitted events (`Swap`, order fills): what
//!   the contract declared happened.
//! - **Calldata** — the transaction's top-level input data.
//!
//! Choosing a trait implementation is choosing which evidence to trust. The decoder holds an
//! ordered list ([`default_strategies`]) and asks each strategy in turn; the first one that
//! returns a flow wins, so a strategy later in the list is the fallback for transactions the
//! earlier ones cannot decode.
//!
//! Everything around the question is shared and lives outside the strategies: matching and
//! vetoes in `matching`, and post-processing (guards, solver attribution, gas, embedded
//! quotes, sandwich detection) in the orchestrator, applied identically to every strategy's
//! output. What belongs in a strategy versus a venue or solver module is laid out in the
//! README's placement rules.

pub(crate) mod netting;

use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::TransactionReceipt,
};
use async_trait::async_trait;

use crate::decoder::{
    registry::Registry,
    transfer_ledger::{NetSwap, TransferLedger},
};

/// A method for recovering the trader's flow from one matched, traced transaction.
///
/// Async because a method may need RPC lookups beyond the transaction itself, e.g. checking
/// an address for contract code.
#[async_trait]
pub(crate) trait DecodeStrategy<P: Provider>: Send + Sync {
    /// Label recorded on the trades this strategy decoded, so the JSONL records say which
    /// method produced each trade (deliberately not a metric label).
    fn name(&self) -> &'static str;

    /// Recover the trader's flow, or `None` when this method cannot decode the transaction.
    async fn decode(&self, ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow>;
}

/// The strategies the decoder tries, in precedence order (most trusted first).
pub(crate) fn default_strategies<P: Provider>() -> Vec<Box<dyn DecodeStrategy<P>>> {
    vec![Box::new(netting::TransferNetting)]
}

/// Ask each strategy in precedence order for the trader's flow. The first that answers wins
/// and later strategies are not consulted; a strategy later in the list therefore only ever
/// sees transactions the earlier ones declined. Returns the winning strategy's name with the
/// flow.
pub(crate) async fn recover_flow<P: Provider>(
    strategies: &[Box<dyn DecodeStrategy<P>>],
    ctx: &mut DecodeContext<'_, P>,
) -> Option<(&'static str, TraderFlow)> {
    for strategy in strategies {
        if let Some(flow) = strategy.decode(ctx).await {
            return Some((strategy.name(), flow));
        }
    }
    None
}

/// Everything a decode strategy may read from one matched transaction.
///
/// The decoder gathers every kind of evidence up front, for every matched transaction,
/// regardless of which strategy will win: the receipt and its logs, the root calldata, and
/// the flattened transfer ledger all arrive here. A strategy that starts needing another
/// input extends this struct instead of the trait.
pub(crate) struct DecodeContext<'a, P> {
    /// RPC access, for strategies that must look beyond the transaction.
    pub provider: &'a P,
    pub registry: &'a Registry,
    /// Cross-block contract-code cache, owned by the decoder.
    pub code_cache: &'a mut HashMap<Address, bool>,
    /// The matched transaction's receipt (sender, logs).
    pub receipt: &'a TransactionReceipt,
    /// The contract the transaction entered through (`tx.to`).
    pub entry_point: Address,
    /// The transaction's flattened value movements.
    pub transfer_ledger: &'a TransferLedger,
    /// The transaction's root calldata. Venues declare their solver in it; some solvers embed
    /// their quote.
    pub input: &'a [u8],
}

/// Which part of the transaction's gas counts as the settled route's cost. Decided by the code
/// that recovers the flow — only it knows who sent the transaction and what wraps the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GasScope {
    /// The trader sent the transaction: all of its gas is the route's cost.
    WholeTransaction,
    /// The trade runs inside a venue's contract: only the solver call's trace frame counts,
    /// keeping the venue's own overhead out of the comparison.
    SolverFrame,
    /// Someone other than the trader paid the gas (maker fills, solver rebalances): none of it
    /// is charged.
    NotCharged,
}

/// The trader's side of a matched transaction: the swap, plus the corrections that make it
/// comparable (venue fees backed out, gas scope).
pub(crate) struct TraderFlow {
    /// The address whose net flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Venue fee taken from the input token, already backed out of `swap.amount_in`.
    pub venue_fee_in: Option<U256>,
    /// Venue fee taken from the output token, already added back into `swap.amount_out`.
    pub venue_fee_out: Option<U256>,
    /// Solver label asserted by the strategy itself (e.g. `MetaMask` declares its
    /// solver in calldata), overriding trace-based attribution.
    pub solver_override: Option<String>,
    /// How the settled route's gas is charged against the settled output.
    pub gas_scope: GasScope,
}

impl TraderFlow {
    pub(crate) fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self {
            tracked,
            swap,
            venue_fee_in: None,
            venue_fee_out: None,
            solver_override: None,
            gas_scope: GasScope::NotCharged,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use alloy::{providers::RootProvider, rpc::client::RpcClient, transports::mock::Asserter};

    use super::*;
    use crate::decoder::test_utils::{addr, receipt, swap, tx_hash};

    /// Always declines.
    struct Declines;

    #[async_trait]
    impl<P: Provider> DecodeStrategy<P> for Declines {
        fn name(&self) -> &'static str {
            "declines"
        }

        async fn decode(&self, _ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow> {
            None
        }
    }

    /// Always decodes a fixed flow.
    struct Wins;

    #[async_trait]
    impl<P: Provider> DecodeStrategy<P> for Wins {
        fn name(&self) -> &'static str {
            "wins"
        }

        async fn decode(&self, _ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow> {
            Some(TraderFlow::without_fees(addr(1), swap(addr(10), 1, addr(11), 2)))
        }
    }

    /// Declines, counting how often it was consulted.
    struct CountsCalls(Arc<AtomicUsize>);

    #[async_trait]
    impl<P: Provider> DecodeStrategy<P> for CountsCalls {
        fn name(&self) -> &'static str {
            "counts"
        }

        async fn decode(&self, _ctx: &mut DecodeContext<'_, P>) -> Option<TraderFlow> {
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }
    }

    async fn recover_with(
        strategies: Vec<Box<dyn DecodeStrategy<RootProvider>>>,
    ) -> Option<(&'static str, TraderFlow)> {
        let provider = RootProvider::new(RpcClient::mocked(Asserter::new()));
        let registry = Registry::ethereum();
        let mut code_cache = HashMap::new();
        let receipt = receipt(tx_hash(1), addr(1), Some(addr(2)), vec![]);
        let transfer_ledger = TransferLedger::from_transaction(&[], &[]);
        let mut ctx = DecodeContext {
            provider: &provider,
            registry: &registry,
            code_cache: &mut code_cache,
            receipt: &receipt,
            entry_point: addr(2),
            transfer_ledger: &transfer_ledger,
            input: &[],
        };
        recover_flow(&strategies, &mut ctx).await
    }

    #[tokio::test]
    async fn first_strategy_declines() {
        let (name, flow) = recover_with(vec![Box::new(Declines), Box::new(Wins)])
            .await
            .unwrap();
        assert_eq!(name, "wins");
        assert_eq!(flow.tracked, addr(1));
    }

    #[tokio::test]
    async fn first_strategy_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (name, _) =
            recover_with(vec![Box::new(Wins), Box::new(CountsCalls(Arc::clone(&calls)))])
                .await
                .unwrap();
        assert_eq!(name, "wins");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn no_strategy_answers() {
        assert!(recover_with(vec![Box::new(Declines)])
            .await
            .is_none());
    }
}
