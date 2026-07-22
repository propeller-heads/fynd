//! Order-flow client attribution.
//!
//! A trade's venue is normally the contract the trader entered through (`tx.to`). Some clients own
//! the order flow without being that contract — kpk's Safes settle through `CoW`, so `tx.to` is
//! the solver and the client is only visible as the order's owner. This module recognizes those
//! clients from the decoded flow and overrides the venue label accordingly.
//!
//! Fingerprints are added here as clients need them: the owning trader address today; fee-wallet
//! and provider-integrator fingerprints follow the same override.

use crate::decoder::{decode::TraderFlow, registry::Registry};

/// The order-flow client for a decoded flow, when a fingerprint matches. Today the only fingerprint
/// is the owning trader address (`registry.client_for_owner`) — the client whose Safe or account
/// the swap's net flow was read from.
pub(crate) fn attribute(registry: &Registry, flow: &TraderFlow) -> Option<String> {
    registry
        .client_for_owner(flow.tracked)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::test_utils::{addr, swap};

    #[test]
    fn test_attributes_owner_to_client() {
        // A CoW-settled kpk trade nets to the Safe that owns the order; the client is that Safe.
        let registry = Registry::ethereum();
        let kpk_safe = address!("0x4f2083f5fbede34c2714affb3105539775f7fe64");
        let flow = TraderFlow::without_fees(kpk_safe, swap(addr(10), 1, addr(11), 2));
        assert_eq!(attribute(&registry, &flow).as_deref(), Some("kpk"));
    }

    #[test]
    fn test_unknown_owner_is_not_a_client() {
        let registry = Registry::ethereum();
        let flow = TraderFlow::without_fees(addr(9), swap(addr(10), 1, addr(11), 2));
        assert_eq!(attribute(&registry, &flow), None);
    }
}
