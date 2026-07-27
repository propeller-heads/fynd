//! Solver-specific knowledge: the routers Fynd competes with.
//!
//! Solver addresses live in the address book's `[solvers]` section, and for most solvers that
//! line is all that is needed: matching, attribution, and gas isolation work from the address
//! alone. A solver whose transactions carry more information than that gets a module here with a
//! `SolverKnowledge` impl registered in `IMPLEMENTATIONS`: an off-chain quote embedded in
//! calldata, or a matching veto for order shapes that are not same-chain swaps.

pub(crate) mod attribution;
pub(crate) mod kyberswap;
pub(crate) mod lifi;
pub(crate) mod paraswap;
pub(crate) mod zeroex;

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
};

use crate::decoder::{registry::Registry, veto::Veto};

/// A solver's own off-chain quote for the swap, recovered from calldata.
///
/// This is the number the venue compared against at decision time — what the solver's API
/// promised — as opposed to the settled amount, which is what execution delivered. The fields
/// carry no solver name; the record's `solver` column already says who.
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

/// Solver-specific knowledge beyond the address-book entry.
///
/// Every method has a default meaning "this solver has nothing to add", so a solver only
/// implements the capabilities it has; most solvers need no code at all.
pub(crate) trait SolverKnowledge: Send + Sync {
    /// The solver's off-chain quote declared in the transaction's calldata, when it embeds one.
    fn embedded_quote(&self, _input: &[u8], _amount_in: U256) -> Option<SolverQuote> {
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
}

/// The solvers with a `SolverKnowledge` implementation, by address-book name. A solver absent
/// here needs none — its address-book entry alone is complete.
const IMPLEMENTATIONS: &[(&str, &'static dyn SolverKnowledge)] = &[
    ("0x", &zeroex::ZeroEx),
    ("kyberswap", &kyberswap::Kyberswap),
    ("lifi", &lifi::Lifi),
    ("paraswap", &paraswap::Paraswap),
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

/// The solver's off-chain quote declared in the transaction's calldata, when the attributed
/// solver is known to embed one.
pub(crate) fn embedded_quote(solver: &str, input: &[u8], amount_in: U256) -> Option<SolverQuote> {
    let (_, knowledge) = IMPLEMENTATIONS
        .iter()
        .find(|(name, _)| *name == solver)?;
    knowledge.embedded_quote(input, amount_in)
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
    fn test_embedded_quote_dispatch() {
        // A ParaSwap-shaped word triple only parses when the attributed solver is paraswap;
        // an unlisted solver never yields a quote from the same bytes.
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
