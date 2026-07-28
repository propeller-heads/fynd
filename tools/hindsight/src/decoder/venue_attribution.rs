//! Order-flow venue attribution.
//!
//! A trade's venue is normally the contract the trader entered through (`tx.to`). Some venues own
//! the order flow without being that contract, and are recognized here from the decoded flow —
//! overriding the entry-point label. Every fingerprint is registry-driven; nothing here knows
//! about a specific venue or provider.
//!
//! Four fingerprints, tried in order:
//! - **owning trader** — the swap's net flow was read from a known venue address
//!   (`[venue_owners]`).
//! - **`CoW` appData tag** — the settled order committed a frontend tag (`appCode`) whose appData
//!   hash maps to a venue (`[venue_appdata]`). The hash is extracted by the caller (it is
//!   `CoW`-specific; see `crate::decoder::intents::cow::order_app_data`), so this module stays
//!   generic.
//! - **fee wallet** — a known venue fee wallet received the output-token fee (`[venue_fees]`); the
//!   fee is backed out so the settled output stays gross. Checked only inside an already-matched
//!   solver trade, so a bare dust transfer to a fee wallet is not mistaken for flow.
//! - **provider integrator tag** — a provider's event carried an integrator string mapped to a
//!   venue (`[venue_integrators]`). The tag is extracted by the caller (it is provider-specific;
//!   see `crate::decoder::solvers::integrator`), so this module stays generic.

use std::collections::HashSet;

use alloy::primitives::{Address, B256, U256};

use crate::decoder::{decode::TraderFlow, registry::Registry, transfer_ledger::TransferLedger};

/// The order-flow venue for a decoded flow, when a fingerprint matches. On a fee-wallet match the
/// venue's fee is backed out of `flow` (added to the gross output) unless a venue decoder already
/// accounted a fee.
pub(crate) fn attribute(
    registry: &Registry,
    flow: &mut TraderFlow,
    ledger: &TransferLedger,
    integrator: Option<&str>,
    app_data: Option<B256>,
) -> Option<String> {
    if let Some(venue) = registry.venue_for_owner(flow.tracked) {
        return Some(venue.to_string());
    }
    if let Some(venue) = app_data.and_then(|hash| registry.venue_for_appdata(hash)) {
        return Some(venue.to_string());
    }
    if let Some((venue, fee)) = fee_venue(registry, ledger, flow.swap.token_out) {
        flow.gross_output_fee(fee);
        return Some(venue);
    }
    integrator
        .and_then(|tag| registry.venue_for_integrator(tag))
        .map(str::to_string)
}

/// The venue whose fee wallet received the output token in this trade, with the fee amount.
/// `None` when no venue fee wallet took a non-zero cut of the output token.
fn fee_venue(
    registry: &Registry,
    ledger: &TransferLedger,
    token_out: Address,
) -> Option<(String, U256)> {
    for (wallet, venue) in registry.venue_fees() {
        let fee = ledger
            .received_by(&HashSet::from([*wallet]))
            .get(&token_out)
            .copied()
            .filter(|amount| !amount.is_zero());
        if let Some(fee) = fee {
            return Some((venue.clone(), fee));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256};

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    #[test]
    fn test_attributes_owner_to_venue() {
        // A CoW-settled kpk trade nets to the Safe that owns the order; the venue is that Safe.
        let registry = Registry::ethereum();
        let kpk_safe = address!("0x4f2083f5fbede34c2714affb3105539775f7fe64");
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = TraderFlow::without_fees(kpk_safe, swap(addr(10), 1, addr(11), 2));
        assert_eq!(attribute(&registry, &mut flow, &ledger, None, None).as_deref(), Some("kpk"));
    }

    #[test]
    fn test_unknown_owner_is_not_a_venue() {
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = TraderFlow::without_fees(addr(9), swap(addr(10), 1, addr(11), 2));
        assert_eq!(attribute(&registry, &mut flow, &ledger, None, None), None);
    }

    #[test]
    fn test_appdata_tag_attributes_venue() {
        // A CoW order carrying DefiLlama's appData hash is attributed to LlamaSwap; an unregistered
        // hash is not.
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let defillama = b256!("0xf249b3db926aa5b5a1b18f3fec86b9cc99b9a8a99ad7e8034242d2838ae97422");
        let mut flow = TraderFlow::without_fees(addr(1), swap(addr(10), 1, addr(11), 2));
        assert_eq!(
            attribute(&registry, &mut flow, &ledger, None, Some(defillama)).as_deref(),
            Some("llamaswap")
        );
        assert_eq!(attribute(&registry, &mut flow, &ledger, None, Some(B256::ZERO)), None);
    }

    #[test]
    fn test_fee_wallet_attributes_and_grosses_fee_back() {
        // A 0x-routed Phantom swap: the buy-token fee reaches Phantom's wallet. It must be added
        // back so the settled output is gross (else every Phantom swap under-reports by 85 bps).
        let registry = Registry::ethereum();
        let phantom = address!("0x2cffed5d56eb6a17662756ca0fdf350e732c9818");
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(9915)),
            make_transfer_log(token_out, pool, phantom, U256::from(85)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = TraderFlow::without_fees(user, swap(token_in, 1000, token_out, 9915));

        assert_eq!(
            attribute(&registry, &mut flow, &ledger, None, None).as_deref(),
            Some("phantom")
        );
        assert_eq!(flow.venue_fee_out, Some(U256::from(85)));
        assert_eq!(flow.swap.amount_out, U256::from(10000));
    }

    #[test]
    fn test_integrator_tag_attributes_venue() {
        // A provider integrator tag maps to its venue, case-insensitively; an unknown tag does
        // not.
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = TraderFlow::without_fees(addr(1), swap(addr(10), 1, addr(11), 2));
        assert_eq!(
            attribute(&registry, &mut flow, &ledger, Some("Infinex"), None).as_deref(),
            Some("infinex")
        );
        assert_eq!(attribute(&registry, &mut flow, &ledger, Some("somedapp"), None), None);
    }

    #[test]
    fn test_no_fee_transfer_is_not_a_venue() {
        // Dust to the fee wallet in a token other than the output is not this trade's fee.
        let registry = Registry::ethereum();
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, pool, U256::from(1000)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = TraderFlow::without_fees(user, swap(token_in, 1000, token_out, 2000));
        assert_eq!(attribute(&registry, &mut flow, &ledger, None, None), None);
    }
}
