//! Solver-specific knowledge: the routers Fynd competes with.
//!
//! Solver addresses live in the address book's `[solvers]` section, and for most solvers that
//! line is all that is needed: matching, attribution, and gas isolation work from the address
//! alone. A solver whose transactions carry more information than that gets a module here with a
//! `SolverKnowledge` impl registered in `IMPLEMENTATIONS`: a swap intent recovered from calldata,
//! or a matching veto for order shapes that are not same-chain swaps.

pub(crate) mod attribution;
pub(crate) mod fly;
pub(crate) mod kyberswap;
pub(crate) mod lifi;
pub(crate) mod paraswap;
pub(crate) mod zeroex;

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
};

use crate::decoder::{registry::Registry, veto::Veto};

/// A trader's swap terms recovered from a solver frame's own calldata: what the trade moved, the
/// floor the trader would accept, and — when the calldata declares one — the solver's own
/// off-chain quote.
///
/// `token_in`/`token_out`/`amount_in`/`min_amount_out` are the on-chain enforced terms of the
/// swap itself, recovered so a reverted swap can still be judged against its floor, since a
/// revert emits no logs to net a settled amount from. The declared quote is different: it is the
/// number the venue compared against at decision time — what the solver's API promised — as
/// opposed to the settled amount, which is what execution delivered. It is self-reported and not
/// every solver declares one, so it is read through [`SwapIntent::declared_quote`], `None` when
/// the calldata carried no quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) struct SwapIntent {
    /// `Address::ZERO` for native ETH.
    pub token_in: Address,
    /// `Address::ZERO` for native ETH.
    pub token_out: Address,
    pub amount_in: U256,
    /// The trader's on-chain enforced floor for `token_out` — the swap reverts below it.
    pub min_amount_out: U256,
    /// The solver's declared off-chain quote, when its calldata carries one. Private: the
    /// ABI-decoded fields above are hard facts, this one is self-reported, so it is read only
    /// through the accessors, never assumed present.
    quoted_amount_out: Option<U256>,
    /// Unix timestamp of the declared quote, when present. Only `KyberSwap`'s `clientData`
    /// exposes one.
    pub timestamp: Option<u64>,
}

impl SwapIntent {
    /// A swap intent with just the ABI-enforced terms: token in/out, amount in, and the on-chain
    /// floor. No declared quote or timestamp — attach one with [`SwapIntent::with_quote`].
    pub(crate) fn new(
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        min_amount_out: U256,
    ) -> Self {
        Self {
            token_in,
            token_out,
            amount_in,
            min_amount_out,
            quoted_amount_out: None,
            timestamp: None,
        }
    }

    /// Attach the solver's declared off-chain quote and, when known, its timestamp.
    pub(crate) fn with_quote(mut self, quoted_amount_out: U256, timestamp: Option<u64>) -> Self {
        self.quoted_amount_out = Some(quoted_amount_out);
        self.timestamp = timestamp;
        self
    }

    /// The solver's declared off-chain quote, `None` when the calldata carried none.
    pub(crate) fn declared_quote(&self) -> Option<U256> {
        self.quoted_amount_out
    }

    /// Drop the declared quote, keeping the ABI-enforced terms. Used when the settled amount
    /// shows the quote was self-reported garbage (see [`plausible_quote`]) — the ABI fields stay
    /// trustworthy either way.
    pub(crate) fn clear_quote(&mut self) {
        self.quoted_amount_out = None;
    }
}

/// Solver-specific knowledge beyond the address-book entry.
///
/// Every method has a default meaning "this solver has nothing to add", so a solver only
/// implements the capabilities it has; most solvers need no code at all.
pub(crate) trait SolverKnowledge: Send + Sync {
    /// The swap terms encoded in the solver frame's own calldata, when this solver's calldata
    /// carries them plainly enough to recover without netting a settled amount. Dispatched with
    /// the solver frame's input (found via `trace::find_solver_frame`/the reverted-tolerant
    /// variant), not the root transaction's — a packed calldata layout (Fly) uses offsets valid
    /// only in its own frame.
    ///
    /// `amount_in_hint` is the decoded flow's input amount, when one is known — absent for a
    /// reverted trade, which has no netted flow to draw it from. Some extractors (`ParaSwap`) need
    /// it to locate fields by value rather than by ABI offset.
    fn swap_intent(&self, _input: &[u8], _amount_in_hint: Option<U256>) -> Option<SwapIntent> {
        None
    }

    /// The address this solver's calldata declares as the output recipient, when it carries one
    /// plainly enough to recover — how a calldata-primary decode learns whose receipt to read the
    /// settled amount from, since calldata alone never carries a settled amount. Dispatched with
    /// the same solver-frame input as `swap_intent`. `None` when the calldata carries no such
    /// field (most solvers deliver to the caller implicitly) or it did not parse.
    fn output_recipient(&self, _input: &[u8]) -> Option<Address> {
        None
    }

    /// The veto this solver's logs place on a matched transaction that is not decodable as a
    /// swap. Checked at match time — before attribution names the solver, and before the
    /// transaction costs a trace.
    fn solver_veto(&self, _logs: &[Log]) -> Option<Veto> {
        None
    }

    /// The order-flow integrator tag this solver records in its logs, when it exposes one. A
    /// solver that fronts other apps (`LiFi`'s Diamond) carries the frontend's integrator string in
    /// its swap event; venue attribution maps that tag to a venue (see
    /// `crate::decoder::venue_attribution`).
    fn integrator(&self, _logs: &[Log]) -> Option<String> {
        None
    }

    /// Whether a reverted call frame's output or revert reason matches this solver's
    /// slippage-floor marker — the avoidable class of revert a fresher quote could have cleared
    /// (Fly's `InsufficientAmountOut()` selector, `KyberSwap`'s "Return amount is not enough"
    /// revert reason). Checked against every frame in a reverted trace's subtree (see
    /// `trace::classify_revert_cause`), so a solver need not be attributed yet to be recognized.
    fn is_slippage_floor(&self, _output: Option<&[u8]>, _revert_reason: Option<&str>) -> bool {
        false
    }
}

/// The solvers with a `SolverKnowledge` implementation, by address-book name. A solver absent
/// here needs none — its address-book entry alone is complete.
const IMPLEMENTATIONS: &[(&str, &'static dyn SolverKnowledge)] = &[
    ("fly", &fly::Fly),
    ("kyberswap", &kyberswap::Kyberswap),
    ("lifi", &lifi::Lifi),
    ("paraswap", &paraswap::Paraswap),
    ("0x", &zeroex::ZeroEx),
];

/// The veto a solver places on a matched transaction that must be skipped instead of decoded,
/// if any.
///
/// Some solver routers also settle orders that are not same-chain swaps; decoding those would
/// record trades that never happened. A solver's veto is consulted only when that solver is
/// part of the transaction — as its entry point or as a log emitter — so a veto can never
/// affect another solver's trades.
pub(crate) fn solver_veto(logs: &[Log], entry_point: Address, registry: &Registry) -> Option<Veto> {
    for (name, knowledge) in IMPLEMENTATIONS {
        let present = registry.solver_name(entry_point) == Some(name) ||
            logs.iter()
                .any(|log| registry.solver_name(log.address()) == Some(name));
        if present {
            if let Some(veto) = knowledge.solver_veto(logs) {
                return Some(veto);
            }
        }
    }
    None
}

/// The order-flow integrator tag declared in a transaction's logs, from whichever solver records
/// one. Only a solver that fronts other apps (`LiFi`) returns a tag; the rest default to `None`, so
/// the first hit is the answer.
pub(crate) fn integrator(logs: &[Log]) -> Option<String> {
    IMPLEMENTATIONS
        .iter()
        .find_map(|(_, knowledge)| knowledge.integrator(logs))
}

/// The swap terms encoded in the solver frame's own calldata, dispatched on the attributed
/// solver so a lookalike blob from another router cannot masquerade as an intent.
pub(crate) fn swap_intent(
    solver: &str,
    input: &[u8],
    amount_in_hint: Option<U256>,
) -> Option<SwapIntent> {
    let (_, knowledge) = IMPLEMENTATIONS
        .iter()
        .find(|(name, _)| *name == solver)?;
    knowledge.swap_intent(input, amount_in_hint)
}

/// The address the solver frame's own calldata declares as the output recipient, dispatched on
/// the attributed solver so a lookalike blob from another router cannot masquerade as one.
pub(crate) fn output_recipient(solver: &str, input: &[u8]) -> Option<Address> {
    let (_, knowledge) = IMPLEMENTATIONS
        .iter()
        .find(|(name, _)| *name == solver)?;
    knowledge.output_recipient(input)
}

/// Whether a reverted call frame's output or revert reason matches any registered solver's
/// slippage-floor marker. Unscoped by attribution — the marker is a hard fact about the frame's
/// own bytes, and only one solver's check can ever match a given frame — so every implementation
/// is tried.
pub(crate) fn is_slippage_floor(output: Option<&[u8]>, revert_reason: Option<&str>) -> bool {
    IMPLEMENTATIONS
        .iter()
        .any(|(_, knowledge)| knowledge.is_slippage_floor(output, revert_reason))
}

/// Whether a declared quote is in the same units as the settled output.
///
/// Quotes are self-reported calldata: integrators sometimes fill them in a different token or
/// decimal basis (seen live: quoted 1.2e23 vs settled 1.2e11), which would fabricate a -100%
/// slippage. A real quote and its settlement differ by slippage, never by orders of magnitude,
/// so anything outside a 2x band is dropped rather than recorded.
pub(crate) fn plausible_quote(quoted_amount_out: U256, settled_amount_out: U256) -> bool {
    quoted_amount_out <= settled_amount_out.saturating_mul(U256::from(2)) &&
        settled_amount_out <= quoted_amount_out.saturating_mul(U256::from(2))
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{Bytes, Log as PrimitiveLog},
        sol_types::SolEvent,
    };

    use super::*;
    use crate::decoder::{
        registry::Registry,
        test_utils::{addr, make_transfer_log},
    };

    #[test]
    fn test_implementation_names_against_the_address_book() {
        // A typo'd name here would compile and silently never match, so the registration list
        // gets the same validation as venue bindings: every name must exist in the book.
        let registry = Registry::ethereum();
        for (name, _) in IMPLEMENTATIONS {
            assert!(
                registry.is_solver_name(name),
                "IMPLEMENTATIONS entry '{name}' is not a solver name in the address book"
            );
        }
    }

    /// A bridge-shaped log emitted by the registered `LiFi` router.
    fn bridge_log(emitter: Address) -> Log {
        let primitive = PrimitiveLog::new_unchecked(
            emitter,
            vec![lifi::LiFiTransferStarted::SIGNATURE_HASH],
            Bytes::default(),
        );
        Log { inner: primitive, ..Default::default() }
    }

    #[test]
    fn test_solver_veto_bridge_orders() {
        let registry = Registry::ethereum();
        let lifi_router: Address = "0x1231deb6f5749ef6ce6943a275a1d3e7486f4eae"
            .parse()
            .unwrap();
        let bridge_logs = vec![bridge_log(lifi_router)];
        assert_eq!(solver_veto(&bridge_logs, addr(1), &registry), Some(Veto::BridgeOrder));

        let swap_logs = vec![make_transfer_log(addr(10), addr(1), addr(2), U256::from(1000))];
        assert_eq!(solver_veto(&swap_logs, lifi_router, &registry), None);
    }

    #[test]
    fn test_solver_veto_scoped_to_the_solver_present() {
        // The same bridge-shaped log from an address that is not the LiFi router: LiFi is not
        // part of the transaction, so its veto is never consulted.
        let registry = Registry::ethereum();
        let bridge_logs = vec![bridge_log(addr(70))];
        assert_eq!(solver_veto(&bridge_logs, addr(1), &registry), None);
    }

    #[test]
    fn test_plausible_quote_slippage_and_unit_mismatch() {
        // The audited Relay+KyberSwap trade: quoted 70,400.41, settled 69,996.28 — 57bps of
        // slippage, kept.
        assert!(plausible_quote(U256::from(70_400_409_935u64), U256::from(69_996_280_564u64)));
        // Seen live via Instadapp: quoted in 18-decimal units, settled in 6 — dropped.
        assert!(!plausible_quote(
            U256::from(120_001_117_253_254_637_416_284u128),
            U256::from(120_000_000_000u64)
        ));
    }

    #[test]
    fn test_swap_intent_dispatch_scoped_to_the_attributed_solver() {
        // A ParaSwap-shaped calldata (token pair, then the fromAmount/toAmount/quotedAmount
        // triple) only parses into an intent when the attributed solver is paraswap; an unlisted
        // solver never yields one from the same bytes.
        let amount_in = U256::from(171_521_496u64);
        let mut input = vec![0xe3u8, 0xea, 0xd5, 0x9e];
        for word in [
            U256::from(0x1111u64), // srcToken
            U256::from(0x2222u64), // destToken
            amount_in,
            U256::from(171_430_663u64),
            U256::from(171_602_266u64),
        ] {
            input.extend_from_slice(&word.to_be_bytes::<32>());
        }
        assert!(swap_intent("paraswap", &input, Some(amount_in)).is_some());
        assert!(swap_intent("1inch", &input, Some(amount_in)).is_none());
    }
}
