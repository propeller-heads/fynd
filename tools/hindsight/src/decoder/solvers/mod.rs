//! Solver-specific calldata knowledge: the routers Fynd competes with.
//!
//! A module here holds one solver's quirks — the off-chain quote it embeds in calldata
//! (kyberswap, paraswap) or a matching veto for order shapes that are not same-chain swaps
//! (lifi's bridge orders). Solver addresses live in the address book's `[solvers]` section.

pub(crate) mod attribution;
pub(crate) mod kyberswap;
pub(crate) mod lifi;
pub(crate) mod paraswap;

use alloy::primitives::U256;

/// A solver's own off-chain quote for the swap, recovered from calldata.
///
/// This is the number the client compared against at decision time — what the solver's API
/// promised — as opposed to the settled amount, which is what execution delivered. The fields
/// carry no solver name (the record's `solver` column already says who); see
/// [`embedded_quote`] for which solvers declare one and how.
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

/// The solver's off-chain quote declared in the transaction's calldata, when the attributed
/// solver is known to embed one. Adding a solver is one match arm.
pub(crate) fn embedded_quote(solver: &str, input: &[u8], amount_in: U256) -> Option<SolverQuote> {
    match solver {
        "kyberswap" => kyberswap::embedded_quote(input),
        "paraswap" => paraswap::embedded_quote(input, amount_in),
        _ => None,
    }
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
    use super::*;

    #[test]
    fn plausible_quote_accepts_slippage_and_rejects_unit_mismatch() {
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
    fn embedded_quote_dispatches_by_solver() {
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
