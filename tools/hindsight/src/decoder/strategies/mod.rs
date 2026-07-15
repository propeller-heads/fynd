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
//! output.

pub(crate) mod netting;

use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::TransactionReceipt,
};
use async_trait::async_trait;

use crate::decoder::{
    ledger::{NetSwap, TransferLedger},
    registry::Registry,
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
    async fn decode(&self, ctx: &mut DecodeContext<'_, P>) -> Option<Flow>;
}

/// The strategies the decoder tries, in precedence order (most trusted first).
pub(crate) fn default_strategies<P: Provider>() -> Vec<Box<dyn DecodeStrategy<P>>> {
    vec![Box::new(netting::TransferNetting)]
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
    pub ledger: &'a TransferLedger,
    /// Root calldata of the transaction (venue declarations, embedded quotes).
    pub input: &'a [u8],
}

/// How the settled route's gas is charged against the settled output. Decided by whichever code
/// recovers the flow, since only it knows who sent the transaction and what wraps the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GasScope {
    /// The tracked trader sent the transaction, so the whole receipt's gas is the route's cost.
    Receipt,
    /// The trade entered through a venue's own contract, so only the solver call's trace frame
    /// is the route's cost — the venue's overhead is charged whichever solver it picks and must
    /// stay out of the comparison.
    SolverFrame,
    /// Someone other than the tracked trader paid the gas (maker fills, solver rebalances), so
    /// nothing is deducted.
    NotCharged,
}

/// The decoded user flow of a matched transaction.
pub(crate) struct Flow {
    /// The address whose net flow the swap was read from.
    pub tracked: Address,
    pub swap: NetSwap,
    /// Venue fee taken from the input token, already backed out of `swap.amount_in`.
    pub venue_fee: Option<U256>,
    /// Venue fee taken from the output token, already added back into `swap.amount_out`.
    pub venue_fee_out: Option<U256>,
    /// Solver label asserted by the strategy itself (e.g. `MetaMask` declares its
    /// solver in calldata), overriding trace-based attribution.
    pub solver_override: Option<String>,
    /// How the settled route's gas is charged against the settled output.
    pub gas_scope: GasScope,
}

impl Flow {
    pub(crate) fn without_fees(tracked: Address, swap: NetSwap) -> Self {
        Self {
            tracked,
            swap,
            venue_fee: None,
            venue_fee_out: None,
            solver_override: None,
            gas_scope: GasScope::NotCharged,
        }
    }
}
