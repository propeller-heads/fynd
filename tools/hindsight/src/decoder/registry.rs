//! The decoder's address book: which contracts are solvers, clients, batch settlers, and
//! venue infrastructure on one chain.
//!
//! The data is pure configuration and lives in TOML — the built-in Ethereum book is embedded
//! from `registry/ethereum.toml`; `--registry <path>` loads a modified or per-chain book without
//! recompiling. This module only holds the lookups the decode strategies ask
//! ([`Registry::is_solver`], [`Registry::is_batch_settler`], [`Registry::label`], …).

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use alloy::primitives::Address;
use anyhow::Context;
use serde::Deserialize;

/// The built-in Ethereum address book, embedded at compile time (validated by tests, so it
/// cannot fail to parse at runtime).
const ETHEREUM_TOML: &str = include_str!("registry/ethereum.toml");

/// On-disk shape of a chain's address book. Field meanings are documented in
/// `registry/ethereum.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddressBook {
    wrapped_native: Address,
    batch_settlers: HashSet<Address>,
    solvers: HashMap<Address, String>,
    clients: HashMap<String, ClientAddresses>,
    labels: HashMap<Address, String>,
}

/// A client's addresses on one chain: the contracts users enter through and the collectors its
/// fee skims land on. Keyed by client name in the book; the name binds to a decode strategy at
/// load time (see [`crate::decoder::clients::client_strategy`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientAddresses {
    pub entry_points: HashSet<Address>,
    pub fee_collectors: HashSet<Address>,
}

/// Per-chain address book for trade decoding, loaded from TOML (see the module docs).
#[derive(Debug)]
pub(crate) struct Registry {
    /// Solver routers — the venue that actually settles a swap.
    solvers: HashMap<Address, String>,
    /// Every known address (solvers and clients), for name resolution.
    names: HashMap<Address, String>,
    /// Batch-settlement venues where `tx.to` is the settlement contract and
    /// the transaction sender is a solver, not the trader.
    batch_settlers: HashSet<Address>,
    /// Display names for entry points that are neither clients nor venues — market-maker
    /// fillers, solver contracts, bot routers. Label-only: these must NOT be in `names`,
    /// because `is_known` drives strategy selection and filler-entered transactions have to
    /// keep matching via their solver logs (Maker), not sender netting.
    labels: HashMap<Address, String>,
    /// The chain's wrapped-native token (e.g. WETH), which appears in flows
    /// only as a wrap/unwrap intermediary.
    wrapped_native: Address,
    /// Client address sets, keyed by the client name from the book.
    clients: HashMap<String, ClientAddresses>,
}

impl Registry {
    /// Load the address book for a chain, or from an explicit TOML file when given (which wins
    /// over the chain name — the file says what it describes).
    pub(crate) fn load(chain: &str, override_path: Option<&Path>) -> anyhow::Result<Self> {
        if let Some(path) = override_path {
            let text = fs::read_to_string(path)
                .with_context(|| format!("failed to read registry file {}", path.display()))?;
            return Self::from_toml(&text)
                .with_context(|| format!("invalid registry file {}", path.display()));
        }
        match chain.to_lowercase().as_str() {
            "ethereum" => Ok(Self::ethereum()),
            other => anyhow::bail!(
                "no built-in decoder address registry for chain '{other}' (only ethereum); \
                 pass --registry with that chain's address book"
            ),
        }
    }

    /// The built-in Ethereum address book.
    pub(crate) fn ethereum() -> Self {
        Self::from_toml(ETHEREUM_TOML).expect("embedded ethereum registry must parse")
    }

    fn from_toml(text: &str) -> anyhow::Result<Self> {
        let book: AddressBook =
            toml::from_str(text).context("failed to parse address book TOML")?;

        // A client section only carries addresses; its behavior is bound by name in code. An
        // unbound name (a typo, or a client with no strategy yet) must fail here — silently
        // never matching would just drop that client's trades.
        for name in book.clients.keys() {
            if crate::decoder::clients::client_strategy(name).is_none() {
                anyhow::bail!(
                    "address book client '{name}' has no decode strategy \
                     (see clients::client_strategy for the recognized names)"
                );
            }
        }

        let mut names = book.solvers.clone();
        for (name, client) in &book.clients {
            for &entry_point in &client.entry_points {
                names.insert(entry_point, name.clone());
            }
        }

        Ok(Self {
            solvers: book.solvers,
            names,
            batch_settlers: book.batch_settlers,
            labels: book.labels,
            wrapped_native: book.wrapped_native,
            clients: book.clients,
        })
    }

    /// Whether the address is a known client or solver.
    pub(crate) fn is_known(&self, address: Address) -> bool {
        self.names.contains_key(&address)
    }

    pub(crate) fn is_solver(&self, address: Address) -> bool {
        self.solvers.contains_key(&address)
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

    /// The named client's address book section, when the book has one.
    pub(crate) fn client(&self, name: &str) -> Option<&ClientAddresses> {
        self.clients.get(name)
    }

    /// The client whose entry point this address is, if any.
    pub(crate) fn client_name(&self, address: Address) -> Option<&str> {
        for (name, client) in &self.clients {
            if client.entry_points.contains(&address) {
                return Some(name.as_str());
            }
        }
        None
    }

    /// Resolve an address to its known name, or its hex address if unknown.
    pub(crate) fn label(&self, address: Address) -> String {
        self.names
            .get(&address)
            .or_else(|| self.labels.get(&address))
            .map_or_else(|| address.to_string(), Clone::clone)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::decoder::test_utils::addr;

    #[test]
    fn embedded_ethereum_book_parses() {
        // `ethereum()` expects; this test is what makes that expectation safe.
        let registry = Registry::ethereum();
        assert!(!registry.solvers.is_empty());
        assert!(!registry
            .client("relay")
            .unwrap()
            .entry_points
            .is_empty());
    }

    #[test]
    fn load_selects_ethereum() {
        assert!(Registry::load("ethereum", None).is_ok());
        assert!(Registry::load("Ethereum", None).is_ok());
        assert!(Registry::load("base", None).is_err());
    }

    #[test]
    fn load_reports_unreadable_and_invalid_files() {
        let missing = Registry::load("ethereum", Some(Path::new("/nonexistent/book.toml")));
        assert!(missing
            .unwrap_err()
            .to_string()
            .contains("failed to read registry file"));

        let dir = std::env::temp_dir().join("hindsight_registry_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("invalid.toml");
        std::fs::write(&path, "wrapped_native = \"not an address\"").unwrap();
        let invalid = Registry::load("ethereum", Some(&path));
        assert!(invalid
            .unwrap_err()
            .to_string()
            .contains("invalid registry file"));
    }

    #[test]
    fn unknown_toml_key_is_rejected() {
        // A typo'd section must fail loudly, not silently drop addresses.
        let text = format!("{ETHEREUM_TOML}\n[solverz]\n");
        assert!(Registry::from_toml(&text).is_err());
    }

    #[test]
    fn registries_named() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        assert!(registry.is_solver(oneinch));
        assert!(registry.is_known(relay));
        assert!(!registry.is_solver(relay));
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
    fn client_sections_resolve_by_name_and_entry_point() {
        let registry = Registry::ethereum();
        let relay = registry.client("relay").unwrap();
        let collector = address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef");
        let router = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");
        assert!(relay
            .fee_collectors
            .contains(&collector));
        // The router that performs the skim is a Relay entry point, not the collector itself.
        assert!(!relay.fee_collectors.contains(&router));
        assert!(relay.entry_points.contains(&router));

        assert_eq!(registry.client_name(router), Some("relay"));
        assert_eq!(
            registry.client_name(address!("0x881d40237659c251811cec9c364ef91dc08d300c")),
            Some("metamask")
        );
        // A fee collector is not an entry point; a solver is not a client.
        assert_eq!(registry.client_name(collector), None);
        assert!(registry.client("kyberswap").is_none());
    }

    #[test]
    fn client_without_strategy_is_rejected() {
        // A client section whose name has no decode strategy would silently never match, so the
        // book must fail to load.
        let text =
            format!("{ETHEREUM_TOML}\n[clients.reiay]\nentry_points = []\nfee_collectors = []\n");
        let err = Registry::from_toml(&text)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no decode strategy"), "unexpected error: {err}");
    }

    #[test]
    fn label_known_and_unknown() {
        let registry = Registry::ethereum();
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let unknown = addr(123);
        assert_eq!(registry.label(relay), "relay");
        assert_eq!(registry.label(unknown), unknown.to_string());
    }

    #[test]
    fn display_labels_resolve_without_becoming_known() {
        // Solver/filler labels are display-only: is_known drives strategy selection, and a
        // filler-entered tx must keep matching via its solver logs (Maker), not as a
        // known-client Sender flow.
        let registry = Registry::ethereum();
        let rizzolver = address!("0x225a38bc71102999dd13478bfabd7c4d53f2dc17");
        assert_eq!(registry.label(rizzolver), "rizzolver");
        assert!(!registry.is_known(rizzolver));
        assert!(!registry.is_solver(rizzolver));
    }
}
