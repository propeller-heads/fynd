use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use alloy::primitives::{address, Address};

/// Known aggregator routers — the venue that actually settles a swap.
/// Start with top aggregators — expand later.
static KNOWN_AGGREGATORS: LazyLock<HashMap<Address, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        // 1inch v6 and v5 Aggregation Routers
        (address!("0x111111125421ca6dc452d289314280a0f8842a65"), "1inch"),
        (address!("0x1111111254eeb25477b68fb85ed929f73a960582"), "1inch"),
        // 0x Exchange Proxy (legacy) and AllowanceHolder (Settler)
        (address!("0xdef1c0ded9bec7f1a1670819833240f027b25eff"), "0x"),
        (address!("0x0000000000001ff3684f28c67538d4d072c22734"), "0x"),
        // CoW Protocol Settlement
        (address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41"), "cow"),
        // ParaSwap Augustus v6.2
        (address!("0x6a000f20005980200259b80c5102003040001068"), "paraswap"),
        // Uniswap Universal Router
        (address!("0x3fc91a3afd70395cd496c647d5a6cc9d4b2b7fad"), "uniswap"),
        // UniswapX Dutch order reactor (filler-initiated; found via its log)
        (address!("0x00000011f84b9aa48e5f8aa8b9897600006289be"), "uniswapx"),
        // OKX DEX Router
        (address!("0xf3de3c0d654fda23dad170f0f320a92172509127"), "okx"),
        // Tycho (PropellerHeads) router — current, v2, and prior deployment
        (address!("0x1f8db310f32d48b6180ff902ec60c586128cef47"), "tycho"),
        (address!("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35"), "tycho"),
        (address!("0xda892c989d07a18b5dd3f392d949f00df15c5736"), "tycho"),
    ])
});

pub(crate) fn known_aggregators() -> &'static HashMap<Address, &'static str> {
    &KNOWN_AGGREGATORS
}

/// Known client contracts — the platform that initiates a trade and routes
/// it through an aggregator. The settling aggregator is found in the trace.
static KNOWN_CLIENTS: LazyLock<HashMap<Address, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        // Relay routers (v2 / v2.1 / v3, Cancun) and approval proxies
        (address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222"), "relay"),
        (address!("0x3ec130b627944cad9b2750300ecb0a695da522b6"), "relay"),
        (address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f"), "relay"),
        (address!("0xccc88a9d1b4ed6b0eaba998850414b24f1c315be"), "relay"),
        (address!("0x58cc3e0aa6cd7bf795832a225179ec2d848ce3e7"), "relay"),
    ])
});

pub(crate) fn known_clients() -> &'static HashMap<Address, &'static str> {
    &KNOWN_CLIENTS
}

/// Known client fee-collector addresses — where a client (e.g. Relay) skims its fee from the input
/// token before swapping. Any input-token transfer to one of these is backed out of `amount_in`
/// (see [`crate::decoder::net::fee_to_collectors`]) so the re-solve does not credit Fynd with the
/// client's fee.
pub(crate) fn known_fee_collectors() -> HashSet<Address> {
    HashSet::from([
        // Relay fee collector (input-side skim by the Relay router). Sole collector across a 25-tx
        // on-chain sample; fee ranges ~1–41 bps depending on Relay's fee tier.
        address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef"),
    ])
}

/// Whether `address` is a batch-settlement venue where `tx.to` is the settlement contract and the
/// transaction sender is a solver, not the trader.
///
/// Such trades must be decoded by finding the order maker (like a filler-initiated intent fill),
/// not by tracking the sender: a solver settles many orders at once, so its net flow is not a
/// single swap even when it happens to net to one token on each side.
pub(crate) fn is_batch_settler(address: &Address) -> bool {
    // CoW Protocol Settlement.
    const COW_SETTLEMENT: Address = address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41");
    *address == COW_SETTLEMENT
}

/// All known addresses, for resolving an address to a human name.
static KNOWN_NAMES: LazyLock<HashMap<Address, &'static str>> = LazyLock::new(|| {
    let mut names = known_aggregators().clone();
    names.extend(
        known_clients()
            .iter()
            .map(|(&k, &v)| (k, v)),
    );
    names
});

pub(crate) fn known_names() -> &'static HashMap<Address, &'static str> {
    &KNOWN_NAMES
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
    use crate::decoder::test_utils::addr;

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
    fn cow_is_a_batch_settler() {
        let cow = address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41");
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        assert!(is_batch_settler(&cow));
        assert!(!is_batch_settler(&oneinch));
        assert!(!is_batch_settler(&addr(123)));
    }

    #[test]
    fn relay_fee_collector_is_known() {
        let collector = address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef");
        assert!(known_fee_collectors().contains(&collector));
        // The router that performs the skim is a known Relay client, not the collector itself.
        assert!(!known_fee_collectors()
            .contains(&address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f")));
    }

    #[test]
    fn label_known_and_unknown() {
        let names = known_names();
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let unknown = addr(123);
        assert_eq!(label(relay, names), "relay");
        assert_eq!(label(unknown, names), unknown.to_string());
    }
}
