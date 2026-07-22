//! Order-flow client attribution.
//!
//! A trade's venue is normally the contract the trader entered through (`tx.to`). Some clients own
//! the order flow without being that contract, and are recognized here from the decoded flow —
//! overriding the venue label. Every fingerprint is registry-driven; nothing here knows about a
//! specific client or provider.
//!
//! Three fingerprints, tried in order:
//! - **owning trader** — the swap's net flow was read from a known client address
//!   (`[client_owners]`).
//! - **fee wallet** — a known client fee wallet received the output-token fee (`[client_fees]`);
//!   the fee is backed out so the settled output stays gross. Checked only inside an
//!   already-matched solver trade, so a bare dust transfer to a fee wallet is not mistaken for
//!   flow.
//! - **provider integrator tag** — a provider's event carried an integrator string mapped to a
//!   client (`[client_integrators]`). The tag is extracted by the caller (it is provider-specific;
//!   see `crate::decoder::solvers::integrator`), so this module stays generic.

use std::collections::HashSet;

use alloy::primitives::{Address, U256};

use crate::decoder::{decode::TraderFlow, registry::Registry, transfer_ledger::TransferLedger};

/// The order-flow client for a decoded flow, when a fingerprint matches. On a fee-wallet match the
/// client's fee is backed out of `flow` (added to the gross output) unless a venue decoder already
/// accounted a fee.
pub(crate) fn attribute(
    registry: &Registry,
    flow: &mut TraderFlow,
    ledger: &TransferLedger,
    integrator: Option<&str>,
) -> Option<String> {
    if let Some(client) = registry.client_for_owner(flow.tracked) {
        return Some(client.to_string());
    }
    if let Some((client, fee)) = fee_client(registry, ledger, flow.swap.token_out) {
        if flow.venue_fee_out.is_none() {
            flow.venue_fee_out = Some(fee);
            flow.swap.amount_out = flow.swap.amount_out.saturating_add(fee);
        }
        return Some(client);
    }
    integrator
        .and_then(|tag| registry.client_for_integrator(tag))
        .map(str::to_string)
}

/// The client whose fee wallet received the output token in this trade, with the fee amount.
/// `None` when no client fee wallet took a non-zero cut of the output token.
fn fee_client(
    registry: &Registry,
    ledger: &TransferLedger,
    token_out: Address,
) -> Option<(String, U256)> {
    for (wallet, client) in registry.client_fees() {
        let fee = ledger
            .received_by(&HashSet::from([*wallet]))
            .get(&token_out)
            .copied()
            .filter(|amount| !amount.is_zero());
        if let Some(fee) = fee {
            return Some((client.clone(), fee));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::test_utils::{addr, make_transfer_log, swap};

    #[test]
    fn test_attributes_owner_to_client() {
        // A CoW-settled kpk trade nets to the Safe that owns the order; the client is that Safe.
        let registry = Registry::ethereum();
        let kpk_safe = address!("0x4f2083f5fbede34c2714affb3105539775f7fe64");
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = TraderFlow::without_fees(kpk_safe, swap(addr(10), 1, addr(11), 2));
        assert_eq!(attribute(&registry, &mut flow, &ledger, None).as_deref(), Some("kpk"));
    }

    #[test]
    fn test_unknown_owner_is_not_a_client() {
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = TraderFlow::without_fees(addr(9), swap(addr(10), 1, addr(11), 2));
        assert_eq!(attribute(&registry, &mut flow, &ledger, None), None);
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

        assert_eq!(attribute(&registry, &mut flow, &ledger, None).as_deref(), Some("phantom"));
        assert_eq!(flow.venue_fee_out, Some(U256::from(85)));
        assert_eq!(flow.swap.amount_out, U256::from(10000));
    }

    #[test]
    fn test_integrator_tag_attributes_client() {
        // A provider integrator tag maps to its client, case-insensitively; an unknown tag does
        // not.
        let registry = Registry::ethereum();
        let ledger = TransferLedger::from_transaction(&[], &[]);
        let mut flow = TraderFlow::without_fees(addr(1), swap(addr(10), 1, addr(11), 2));
        assert_eq!(
            attribute(&registry, &mut flow, &ledger, Some("Infinex")).as_deref(),
            Some("infinex")
        );
        assert_eq!(attribute(&registry, &mut flow, &ledger, Some("somedapp")), None);
    }

    #[test]
    fn test_no_fee_transfer_is_not_a_client() {
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
        assert_eq!(attribute(&registry, &mut flow, &ledger, None), None);
    }
}
