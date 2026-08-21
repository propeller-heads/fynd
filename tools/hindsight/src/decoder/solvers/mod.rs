//! Solver-specific decoders: the routers Fynd competes with.
//!
//! Solver addresses live in the address book's `[solvers]` section, and for most solvers that
//! line is all that is needed: matching, attribution, and metric labels work from the address
//! alone. A solver whose calldata or logs carry more than that gets a module here with a
//! `SolverDecoder` impl registered in `IMPLEMENTATIONS`: a `DeclaredSwap` read from its calldata
//! or its own event, or a veto for order shapes that are not same-chain swaps. The impl is joined
//! onto the registry's solver entry once, at address-book load (see `decoder_for`); at trade time
//! every lookup is by address through `Registry::solver`.

pub(crate) mod cow;
pub(crate) mod fly;
pub(crate) mod kyberswap;
pub(crate) mod lifi;
pub(crate) mod okx;
pub(crate) mod oneinch;
pub(crate) mod paraswap;
pub(crate) mod zeroex;

use std::collections::HashSet;

use alloy::{
    primitives::{Address, U256},
    rpc::types::Log,
};

use crate::decoder::veto::Veto;

/// The trade a solver's own data states: always the two tokens and the input amount, plus
/// whatever else its source happens to carry.
///
/// One shape for every solver, whether it was read from calldata or from an event, because the
/// caller does not care which — it cares which fields arrived. A field is `None` when the source
/// could not carry it, so the absence is the instruction:
///
/// - `amount_out` absent means the source stated no output (all calldata does this), so the caller
///   recovers it from `output_recipient`'s receipt in the transfer ledger.
/// - `tracked` absent means the source did not name the trader, so the caller falls back to the
///   transaction sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DeclaredSwap {
    /// `Address::ZERO` for native ETH.
    pub token_in: Address,
    /// `Address::ZERO` for native ETH.
    pub token_out: Address,
    pub amount_in: U256,
    /// The settled output, when the source stated it outright (an event). `None` for calldata,
    /// which never carries one.
    pub amount_out: Option<U256>,
    /// The trader, when the source named them (an event's owner or sender).
    pub tracked: Option<Address>,
    /// The trader's on-chain enforced floor for `token_out` — the swap reverts below it. Recorded
    /// so a reverted swap can still be judged against its floor, since a revert emits no logs to
    /// net a settled amount from.
    pub min_amount_out: Option<U256>,
    /// Who the output is paid to, when the calldata names them — whose receipt `amount_out` is
    /// recovered from when the source stated none.
    pub output_recipient: Option<Address>,
    /// The solver's own off-chain quote: the number the venue compared against at decision time,
    /// as opposed to `amount_out`, which is what execution delivered. Self-reported, and not
    /// every solver declares one.
    pub declared_quote: Option<U256>,
    /// Unix timestamp of `declared_quote`. Only `KyberSwap`'s `clientData` exposes one.
    pub timestamp: Option<u64>,
}

impl DeclaredSwap {
    /// A swap read from a solver's **calldata**: the terms it enforces on-chain. No settled
    /// output — calldata never carries one — so the caller recovers it from the recipient's
    /// receipt. Add the recipient with [`DeclaredSwap::with_recipient`] and an off-chain quote
    /// with [`DeclaredSwap::with_quote`].
    pub(crate) fn from_calldata(
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        min_amount_out: U256,
    ) -> Self {
        Self {
            token_in,
            token_out,
            amount_in,
            amount_out: None,
            tracked: None,
            min_amount_out: Some(min_amount_out),
            output_recipient: None,
            declared_quote: None,
            timestamp: None,
        }
    }

    /// A swap read from a solver's **event**: the trade it already executed, both amounts and the
    /// trader stated outright. Nothing is left to recover. No floor — an event reports what
    /// happened, not what was required.
    pub(crate) fn from_event(
        tracked: Address,
        token_in: Address,
        amount_in: U256,
        token_out: Address,
        amount_out: U256,
    ) -> Self {
        Self {
            token_in,
            token_out,
            amount_in,
            amount_out: Some(amount_out),
            tracked: Some(tracked),
            min_amount_out: None,
            output_recipient: None,
            declared_quote: None,
            timestamp: None,
        }
    }

    /// Attach the output recipient the same calldata declares.
    pub(crate) fn with_recipient(mut self, output_recipient: Address) -> Self {
        self.output_recipient = Some(output_recipient);
        self
    }

    /// Attach the solver's declared off-chain quote and, when known, its timestamp.
    pub(crate) fn with_quote(mut self, declared_quote: U256, timestamp: Option<u64>) -> Self {
        self.declared_quote = Some(declared_quote);
        self.timestamp = timestamp;
        self
    }

    /// The best available "what was promised": the declared quote, or — when absent — the
    /// enforced floor. `None` when the source carried neither.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "only called from tests in this PR; its production caller is the \
                      reverted-swap path in the stacked follow-up PR"
        )
    )]
    pub(crate) fn promised_amount_out(&self) -> Option<U256> {
        self.declared_quote
            .or(self.min_amount_out)
    }
}

/// One solver's decoder: what the solver's own calldata and logs say about a trade.
///
/// One method, defaulted, so a solver only writes code when its transactions expose something;
/// most solvers need none at all. Anything else a solver can read from its own data — a veto, an
/// integrator tag — is an ordinary function in that solver's module, called from here or from the
/// attribution that wants it, not another row on this trait.
///
/// Whether the read came from calldata or from an event is not recorded on the trait: the caller
/// only needs to know which fields arrived, which the `Option`s on [`DeclaredSwap`] already say.
pub(crate) trait SolverDecoder: Send + Sync {
    /// What this solver's own data says about the transaction: `Ok(None)` when it says nothing
    /// this solver can read, `Err(veto)` when it says the transaction is not a swap at all and
    /// must not be decoded by any means (`LiFi`'s cross-chain bridge orders).
    ///
    /// `input` is the solver frame's calldata (found via `trace::find_solver_frame`), not the root
    /// transaction's: a packed layout (Fly) uses offsets valid only in its own frame. `logs` is the
    /// whole receipt's logs, for a solver that states its trade in an event instead. Every solver
    /// reads one or the other; the parameter it does not use is ignored.
    ///
    /// `amount_in_hint` is a netted input amount, when one is known. Some extractors (`ParaSwap`)
    /// need it to locate fields by value rather than by ABI offset.
    fn declared(
        &self,
        _input: &[u8],
        _logs: &[Log],
        _amount_in_hint: Option<U256>,
    ) -> Result<Option<DeclaredSwap>, Veto> {
        Ok(None)
    }

    /// The fee recipients this solver's calldata names, for routers that let an integrator take a
    /// cut of the swap. Only who is paid — `declared_output_fee` reads how much off the ledger.
    fn fee_recipients(&self, _input: &[u8]) -> Vec<Address> {
        Vec::new()
    }
}

/// The solvers with a `SolverDecoder` implementation, by address-book name. A solver absent
/// here needs none — its address-book entry alone is complete. Consulted once, when the address
/// book loads (see `decoder_for`); everything after that calls the trait through the registry
/// entry.
const IMPLEMENTATIONS: &[(&str, &'static dyn SolverDecoder)] = &[
    ("cow", &cow::Cow),
    ("fly", &fly::Fly),
    ("kyberswap", &kyberswap::Kyberswap),
    ("lifi", &lifi::Lifi),
    ("1inch", &oneinch::OneInch),
    ("okx", &okx::Okx),
    ("paraswap", &paraswap::Paraswap),
    ("0x", &zeroex::ZeroEx),
];

/// Substrings of the solver ids venues declare in their calldata, mapped to the address book's
/// solver names. A venue decorates the id it was routed through ("oneInchV6FeeDynamic",
/// "uniswapPermit2FeeDynamic"), and the decoration is the venue's, not the chain's — the same
/// vocabulary on every chain — so it lives here rather than in each chain's address book.
const DECLARED_NAME_ALIASES: &[(&str, &str)] = &[
    ("airswap", "airswap"),
    ("hashflow", "hashflow"),
    ("kyber", "kyberswap"),
    ("okx", "okx"),
    ("oneinch", "1inch"),
    ("openocean", "openocean"),
    ("paraswap", "paraswap"),
    ("uniswap", "uniswap"),
    ("zeroex", "0x"),
];

/// Normalize a solver id a venue declared in its calldata to the address book's solver names:
/// the first alias substring contained in the lowercased id names the solver, trimming the
/// venue's decoration. No alias is a substring of another, so the answer does not depend on the
/// order above. An unmatched id passes through as-is — still more informative than a raw
/// executor address, and a signal to extend the list.
pub(crate) fn normalize_declared_name(id: &str) -> String {
    let lower = id.to_lowercase();
    for (substring, name) in DECLARED_NAME_ALIASES {
        if lower.contains(substring) {
            return (*name).to_string();
        }
    }
    id.to_string()
}

/// A solver with no `SolverDecoder` implementation: every method keeps its "nothing to add"
/// default, so callers hold one handle type and never branch on whether a solver has code.
struct NoDecoder;

impl SolverDecoder for NoDecoder {}

/// Resolve a solver name to its `SolverDecoder`, once, when the address book loads. A book-only
/// solver resolves to the no-op implementation.
pub(crate) fn decoder_for(solver: &str) -> &'static dyn SolverDecoder {
    IMPLEMENTATIONS
        .iter()
        .find(|(name, _)| *name == solver)
        .map_or(&NoDecoder, |(_, decoder)| *decoder)
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
    use super::*;
    use crate::decoder::registry::Registry;

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
    fn test_declared_output_fee_reads_the_integrator_cut() {
        // Base tx 0x78c70ca6…: KyberSwap's calldata names a frontend's wallet, which took 10% of
        // the ETH the trader bought. Without backing it out the settled output is 10% short and
        // Fynd, re-solved gross, wins by 1111 bps on every one of that frontend's trades.
        let collector = addr(41);
        let trader = addr(1);
        let router = addr(50);
        let token_out = Address::ZERO;
        let native = [
            (router, trader, U256::from(45_157_884_343_657_075u64)),
            (router, collector, U256::from(5_017_542_704_850_786u64)),
        ];
        let ledger = TransferLedger::from_transaction(&[], &native);
        let input = kyberswap::swap_calldata(vec![collector]);

        assert_eq!(
            declared_output_fee("kyberswap", &input, &ledger, token_out),
            Some(U256::from(5_017_542_704_850_786u64))
        );
        // Dispatched on the attributed solver: the same calldata under another solver's name
        // declares nothing.
        assert_eq!(declared_output_fee("1inch", &input, &ledger, token_out), None);
        // A swap that names no fee recipient has no fee to back out.
        assert_eq!(
            declared_output_fee(
                "kyberswap",
                &kyberswap::swap_calldata(Vec::new()),
                &ledger,
                token_out
            ),
            None
        );
    }

    #[test]
    fn test_declared_output_fee_ignores_other_tokens() {
        // The recipient was paid, but in a token the trade did not buy — that is not this swap's
        // output fee.
        let collector = addr(41);
        let logs = vec![make_transfer_log(addr(10), addr(50), collector, U256::from(85))];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let input = kyberswap::swap_calldata(vec![collector]);
        assert_eq!(declared_output_fee("kyberswap", &input, &ledger, addr(11)), None);
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
        assert!(decoder_for("paraswap")
            .declared(&input, &[], Some(amount_in))
            .is_ok_and(|declared| declared.is_some()));
        assert!(decoder_for("1inch")
            .declared(&input, &[], Some(amount_in))
            .is_ok_and(|declared| declared.is_none()));
    }
}
