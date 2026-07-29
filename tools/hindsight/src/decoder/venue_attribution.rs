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
//! - **fee wallet** — a known venue fee wallet took a cut of either swap token (`[venue_fees]`);
//!   the fee is backed out, on whichever side it came from, so the settled amounts are what
//!   actually reached the pools. Checked only inside an already-matched solver trade, so a bare
//!   dust transfer to a fee wallet is not mistaken for flow.
//! - **provider integrator tag** — a provider's event carried an integrator string mapped to a
//!   venue (`[venue_integrators]`). The tag is extracted by the caller (it is provider-specific;
//!   see `crate::decoder::solvers::integrator`), so this module stays generic.

use std::collections::HashSet;

use alloy::primitives::{Address, B256, U256};

use crate::decoder::{decode::TraderFlow, registry::Registry, transfer_ledger::TransferLedger};

/// The order-flow venue for a decoded flow, when a fingerprint matches. On a fee-wallet match the
/// venue's fee is backed out of `flow` — added back to the output, or netted out of the input,
/// depending on which side it was taken from — unless a venue decoder already accounted a fee.
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
    if let Some((venue, fee)) = fee_venue(registry, ledger, flow.swap.token_in, flow.swap.token_out)
    {
        match fee {
            VenueFee::Input(amount) => flow.net_input_fee(amount),
            VenueFee::Output(amount) => flow.gross_output_fee(amount),
        }
        return Some(venue);
    }
    integrator
        .and_then(|tag| registry.venue_for_integrator(tag))
        .map(str::to_string)
}

/// Which side of the swap a venue took its fee from, with the amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VenueFee {
    /// Skimmed off the input before the swap, so the settled input is smaller than the user spent.
    Input(U256),
    /// Taken out of the output after the swap, so the settled output is larger than the user kept.
    Output(U256),
}

/// The venue whose fee wallet took a cut of this trade, and which side it came from. `None` when no
/// venue fee wallet received a non-zero amount of either swap token.
///
/// Both sides are checked because venues split on this: Phantom and Robinhood take the buy token,
/// while Coinbase's Base App skims the sell token before routing. The output side is tried first —
/// a wallet that received both tokens is being paid its cut in the token the user bought.
fn fee_venue(
    registry: &Registry,
    ledger: &TransferLedger,
    token_in: Address,
    token_out: Address,
) -> Option<(String, VenueFee)> {
    for (wallet, venue) in registry.venue_fees() {
        let received = ledger.received_by(&HashSet::from([*wallet]));
        let non_zero = |token: &Address| {
            received
                .get(token)
                .copied()
                .filter(|amount| !amount.is_zero())
        };
        if let Some(fee) = non_zero(&token_out) {
            return Some((venue.clone(), VenueFee::Output(fee)));
        }
        if let Some(fee) = non_zero(&token_in) {
            return Some((venue.clone(), VenueFee::Input(fee)));
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
    fn test_fee_wallet_input_side_fee_nets_the_input_down() {
        // A LiFi-routed Coinbase Base App swap: the 0.95% cut is skimmed off the sell token before
        // routing, so only the remainder reached the pools. Leaving it in makes the settled trade
        // look bigger than it was and Fynd, re-solved on that inflated size, appear to win.
        let registry = Registry::load("bsc", None).unwrap();
        let coinbase = address!("0x5aafc1f252d544f744d17a4e734afd6efc47ede4");
        let user = addr(1);
        let pool = addr(50);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, coinbase, U256::from(95)),
            make_transfer_log(token_in, user, pool, U256::from(9905)),
            make_transfer_log(token_out, pool, user, U256::from(2000)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = TraderFlow::without_fees(user, swap(token_in, 10000, token_out, 2000));

        assert_eq!(
            attribute(&registry, &mut flow, &ledger, Some("base-app"), None).as_deref(),
            Some("coinbase")
        );
        assert_eq!(flow.venue_fee_in, Some(U256::from(95)));
        assert_eq!(flow.swap.amount_in, U256::from(9905));
        // The output side is untouched: this venue took nothing out of the buy token.
        assert_eq!(flow.venue_fee_out, None);
        assert_eq!(flow.swap.amount_out, U256::from(2000));
    }

    #[test]
    fn test_fee_wallet_taking_both_tokens_is_read_as_an_output_fee() {
        // A wallet that received both swap tokens is being paid its cut in the token the user
        // bought; the sell-token leg is the swap's own routing, not a second fee.
        let registry = Registry::ethereum();
        let phantom = address!("0x2cffed5d56eb6a17662756ca0fdf350e732c9818");
        let user = addr(1);
        let token_in = addr(10);
        let token_out = addr(11);
        let logs = vec![
            make_transfer_log(token_in, user, phantom, U256::from(7)),
            make_transfer_log(token_out, addr(50), phantom, U256::from(85)),
        ];
        let ledger = TransferLedger::from_transaction(&logs, &[]);
        let mut flow = TraderFlow::without_fees(user, swap(token_in, 1000, token_out, 9915));

        assert_eq!(
            attribute(&registry, &mut flow, &ledger, None, None).as_deref(),
            Some("phantom")
        );
        assert_eq!(flow.venue_fee_out, Some(U256::from(85)));
        assert_eq!(flow.swap.amount_out, U256::from(10000));
        assert_eq!(flow.venue_fee_in, None);
        assert_eq!(flow.swap.amount_in, U256::from(1000));
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
