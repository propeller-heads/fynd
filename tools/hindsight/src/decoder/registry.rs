use std::collections::HashMap;

use alloy::primitives::{address, Address};

/// Known aggregator routers — the venue that actually settles a swap.
/// Start with top aggregators — expand later.
pub(crate) fn known_aggregators() -> HashMap<Address, &'static str> {
    HashMap::from([
        // 1inch v6 Aggregation Router
        (address!("0x111111125421ca6dc452d289314280a0f8842a65"), "1inch"),
        // 0x Exchange Proxy (legacy) and AllowanceHolder (Settler)
        (address!("0xdef1c0ded9bec7f1a1670819833240f027b25eff"), "0x"),
        (address!("0x0000000000001ff3684f28c67538d4d072c22734"), "0x"),
        // CoW Protocol Settlement
        (address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41"), "cow"),
        // ParaSwap Augustus v6.2
        (address!("0x6a000f20005980200259b80c5102003040001068"), "paraswap"),
        // Uniswap Universal Router
        (address!("0x3fc91a3afd70395cd496c647d5a6cc9d4b2b7fad"), "uniswap"),
        // OKX DEX Router
        (address!("0xf3de3c0d654fda23dad170f0f320a92172509127"), "okx"),
    ])
}

/// Known client contracts — the platform that initiates a trade and routes
/// it through an aggregator. The settling aggregator is found in the trace.
pub(crate) fn known_clients() -> HashMap<Address, &'static str> {
    HashMap::from([
        // Relay routers (v2 / v2.1 / v3, Cancun) and approval proxies
        (address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222"), "relay"),
        (address!("0x3ec130b627944cad9b2750300ecb0a695da522b6"), "relay"),
        (address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f"), "relay"),
        (address!("0xccc88a9d1b4ed6b0eaba998850414b24f1c315be"), "relay"),
        (address!("0x58cc3e0aa6cd7bf795832a225179ec2d848ce3e7"), "relay"),
    ])
}

/// All known addresses, for resolving an address to a human name.
pub(crate) fn known_names() -> HashMap<Address, &'static str> {
    let mut names = known_aggregators();
    names.extend(known_clients());
    names
}

/// Resolve an address to its known name, or its hex address if unknown.
pub(crate) fn label(address: Address, names: &HashMap<Address, &'static str>) -> String {
    names
        .get(&address)
        .map_or_else(|| address.to_string(), |name| (*name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_support::addr;

    #[test]
    fn registries_named() {
        let aggregators = known_aggregators();
        assert!(aggregators
            .values()
            .any(|v| *v == "1inch"));
        assert!(aggregators.values().any(|v| *v == "0x"));
        assert!(known_clients()
            .values()
            .any(|v| *v == "relay"));
    }

    #[test]
    fn label_known_and_unknown() {
        let names = known_names();
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let unknown = addr(123);
        assert_eq!(label(relay, &names), "relay");
        assert_eq!(label(unknown, &names), unknown.to_string());
    }
}
