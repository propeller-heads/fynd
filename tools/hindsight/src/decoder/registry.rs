use std::collections::{HashMap, HashSet};

use alloy::primitives::{address, Address};

/// Relay's addresses on one chain: the routers users enter through and the
/// collectors its fee skims land on.
pub(crate) struct RelayAddresses {
    pub routers: HashSet<Address>,
    pub fee_collectors: HashSet<Address>,
}

/// Per-chain address book for trade decoding: which contracts are aggregators,
/// clients, batch settlers, and venue infrastructure.
///
/// Built once per run via [`Registry::for_chain`]. Only Ethereum is populated
/// today; supporting another chain means adding a constructor with that
/// chain's addresses, not touching decode logic.
pub(crate) struct Registry {
    /// Aggregator routers — the venue that actually settles a swap.
    aggregators: HashMap<Address, &'static str>,
    /// Every known address (aggregators and clients), for name resolution.
    names: HashMap<Address, &'static str>,
    /// Batch-settlement venues where `tx.to` is the settlement contract and
    /// the transaction sender is a solver, not the trader.
    batch_settlers: HashSet<Address>,
    /// The chain's wrapped-native token (e.g. WETH), which appears in flows
    /// only as a wrap/unwrap intermediary.
    wrapped_native: Address,
    relay: RelayAddresses,
}

impl Registry {
    pub(crate) fn for_chain(chain: &str) -> anyhow::Result<Self> {
        match chain.to_lowercase().as_str() {
            "ethereum" => Ok(Self::ethereum()),
            other => anyhow::bail!(
                "no decoder address registry for chain '{other}' (only ethereum is supported)"
            ),
        }
    }

    pub(crate) fn ethereum() -> Self {
        let aggregators = HashMap::from([
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
        ]);

        // Client contracts — the platform that initiates a trade and routes it
        // through an aggregator found in the trace.
        let clients: HashMap<Address, &'static str> = HashMap::from([
            // Relay routers (v2 / v2.1 / v3, Cancun) and approval proxies
            (address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222"), "relay"),
            (address!("0x3ec130b627944cad9b2750300ecb0a695da522b6"), "relay"),
            (address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f"), "relay"),
            (address!("0xccc88a9d1b4ed6b0eaba998850414b24f1c315be"), "relay"),
            (address!("0x58cc3e0aa6cd7bf795832a225179ec2d848ce3e7"), "relay"),
        ]);

        let relay = RelayAddresses {
            routers: clients
                .iter()
                .filter(|(_, name)| **name == "relay")
                .map(|(address, _)| *address)
                .collect(),
            // Relay fee collector (input-side skim by the Relay router). Sole collector across a
            // 25-tx on-chain sample; fee ranges ~1–41 bps depending on Relay's fee tier.
            fee_collectors: HashSet::from([address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef")]),
        };

        let mut names = aggregators.clone();
        names.extend(clients);

        Self {
            aggregators,
            names,
            // CoW Protocol Settlement.
            batch_settlers: HashSet::from([address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41")]),
            // WETH
            wrapped_native: address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            relay,
        }
    }

    /// Whether the address is a known client or aggregator.
    pub(crate) fn is_known(&self, address: Address) -> bool {
        self.names.contains_key(&address)
    }

    pub(crate) fn is_aggregator(&self, address: Address) -> bool {
        self.aggregators.contains_key(&address)
    }

    /// Whether `address` is a batch-settlement venue (e.g. CoW). Such trades
    /// must be decoded by finding the order maker, not by tracking the
    /// sender: a solver settles many orders at once, so its net flow is not a
    /// single swap even when it happens to net to one token on each side.
    pub(crate) fn is_batch_settler(&self, address: Address) -> bool {
        self.batch_settlers.contains(&address)
    }

    pub(crate) fn wrapped_native(&self) -> Address {
        self.wrapped_native
    }

    pub(crate) fn relay(&self) -> &RelayAddresses {
        &self.relay
    }

    /// Resolve an address to its known name, or its hex address if unknown.
    pub(crate) fn label(&self, address: Address) -> String {
        self.names
            .get(&address)
            .map_or_else(|| address.to_string(), |name| (*name).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_utils::addr;

    #[test]
    fn for_chain_selects_ethereum() {
        assert!(Registry::for_chain("ethereum").is_ok());
        assert!(Registry::for_chain("Ethereum").is_ok());
        assert!(Registry::for_chain("base").is_err());
    }

    #[test]
    fn registries_named() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        assert!(registry.is_aggregator(oneinch));
        assert!(registry.is_known(relay));
        assert!(!registry.is_aggregator(relay));
    }

    #[test]
    fn cow_is_a_batch_settler() {
        let registry = Registry::ethereum();
        let cow = address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41");
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        assert!(registry.is_batch_settler(cow));
        assert!(!registry.is_batch_settler(oneinch));
        assert!(!registry.is_batch_settler(addr(123)));
    }

    #[test]
    fn relay_fee_collector_is_known() {
        let registry = Registry::ethereum();
        let collector = address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef");
        assert!(registry
            .relay()
            .fee_collectors
            .contains(&collector));
        // The router that performs the skim is a known Relay client, not the collector itself.
        assert!(!registry
            .relay()
            .fee_collectors
            .contains(&address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f")));
        assert!(registry
            .relay()
            .routers
            .contains(&address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f")));
    }

    #[test]
    fn label_known_and_unknown() {
        let registry = Registry::ethereum();
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let unknown = addr(123);
        assert_eq!(registry.label(relay), "relay");
        assert_eq!(registry.label(unknown), unknown.to_string());
    }
}
