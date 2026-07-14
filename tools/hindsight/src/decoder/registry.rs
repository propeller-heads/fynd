//! The decoder's address book: which contracts are solvers, venues, batch settlers, and
//! venue infrastructure on one chain.
//!
//! The data is pure configuration and lives in TOML — the built-in Ethereum book is embedded
//! from `registry/ethereum.toml`; `--registry <path>` loads a modified or per-chain book without
//! recompiling. This module only holds the lookups the decode strategies ask
//! ([`Registry::is_solver`], [`Registry::is_batch_settler`], [`Registry::label`], …).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
    infrastructure: HashSet<Address>,
    usd_stablecoins: HashMap<Address, u32>,
    batch_settlers: HashSet<Address>,
    solvers: HashMap<Address, String>,
    venues: HashMap<String, VenueAddresses>,
    labels: HashMap<Address, String>,
}

/// A venue's book section on one chain: the contracts users enter through, the collectors its
/// fee skims land on, and its calldata solver vocabulary. Keyed by venue name in the book; the
/// name binds to a decode strategy at load time (see
/// [`crate::decoder::venues::Venue::from_name`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VenueAddresses {
    pub(crate) entry_points: HashSet<Address>,
    pub(crate) fee_collectors: HashSet<Address>,
    /// Lowercase substrings of the venue's calldata solver ids, mapped to the solver name used
    /// in this book. Ordered for deterministic matching; empty for venues that declare no
    /// solver in calldata.
    #[serde(default)]
    solver_aliases: BTreeMap<String, String>,
}

impl VenueAddresses {
    /// Normalize a solver id this venue declared in calldata to the book's solver vocabulary:
    /// the first alias needle (in alias order) contained in the lowercased id names the solver,
    /// trimming the venue's id decoration ("oneInchV6FeeDynamic" → "1inch") — not a 1:1 rename.
    /// Unmatched ids pass through as-is: still more informative than a raw executor address,
    /// and a signal to extend the book.
    pub(crate) fn normalize_solver(&self, id: &str) -> String {
        let lower = id.to_lowercase();
        for (needle, name) in &self.solver_aliases {
            if lower.contains(needle) {
                return name.clone();
            }
        }
        id.to_string()
    }
}

/// Per-chain address book for trade decoding, loaded from TOML (see the module docs).
#[derive(Debug)]
pub(crate) struct Registry {
    /// Solver routers — the venue that actually settles a swap.
    solvers: HashMap<Address, String>,
    /// Display names of all registered solvers, for O(1) `is_solver_name` checks.
    solver_names: HashSet<String>,
    /// Every known address (solvers and venues), for name resolution.
    names: HashMap<Address, String>,
    /// Batch-settlement venues where `tx.to` is the settlement contract and
    /// the transaction sender is a solver, not the trader.
    batch_settlers: HashSet<Address>,
    /// Display names for entry points that are neither venues nor solvers — market-maker
    /// fillers, solver contracts, bot routers. Label-only: these must NOT be in `names`,
    /// because `is_known` drives strategy selection and filler-entered transactions have to
    /// keep matching via their solver logs (Maker), not sender netting.
    labels: HashMap<Address, String>,
    /// The chain's wrapped-native token (e.g. WETH), which appears in flows
    /// only as a wrap/unwrap intermediary.
    wrapped_native: Address,
    /// Token-movement infrastructure (e.g. Permit2): contracts traces touch on most swaps
    /// without ever being the settling venue.
    infrastructure: HashSet<Address>,
    /// `(stablecoin, decimals)` anchors for valuing trades in USD (see `crate::usd`), sorted by
    /// address for deterministic averaging.
    usd_stablecoins: Vec<(Address, u32)>,
    /// Venue address sets, keyed by the venue name from the book.
    venues: HashMap<String, VenueAddresses>,
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
        let mut book: AddressBook =
            toml::from_str(text).context("failed to parse address book TOML")?;

        // A venue section only carries addresses; its behavior is bound by name in code. An
        // unbound name (a typo, or a venue with no strategy yet) must fail here — silently
        // never matching would just drop that venue's trades.
        for name in book.venues.keys() {
            if crate::decoder::venues::Venue::from_name(name).is_none() {
                anyhow::bail!(
                    "address book venue '{name}' has no decode strategy \
                     (see venues::Venue for the recognized names)"
                );
            }
        }

        let mut names = book.solvers.clone();
        for (name, venue) in &book.venues {
            for &entry_point in &venue.entry_points {
                names.insert(entry_point, name.clone());
            }
        }
        let solver_names = book.solvers.values().cloned().collect();
        let mut usd_stablecoins: Vec<(Address, u32)> = book
            .usd_stablecoins
            .into_iter()
            .collect();
        usd_stablecoins.sort_unstable();
        // Alias needles match against lowercased ids, so a mixed-case needle in the book would
        // silently never match — canonicalize at load.
        for venue in book.venues.values_mut() {
            venue.solver_aliases = std::mem::take(&mut venue.solver_aliases)
                .into_iter()
                .map(|(needle, name)| (needle.to_lowercase(), name))
                .collect();
        }

        Ok(Self {
            solver_names,
            solvers: book.solvers,
            names,
            batch_settlers: book.batch_settlers,
            labels: book.labels,
            wrapped_native: book.wrapped_native,
            infrastructure: book.infrastructure,
            usd_stablecoins,
            venues: book.venues,
        })
    }

    /// Whether the address is a known venue or solver.
    pub(crate) fn is_known(&self, address: Address) -> bool {
        self.names.contains_key(&address)
    }

    pub(crate) fn is_solver(&self, address: Address) -> bool {
        self.solvers.contains_key(&address)
    }

    /// Whether `name` is a registered solver's display name. Bounds the metric label
    /// vocabulary: attribution can also produce raw addresses, venue names (fallback tier), and
    /// calldata-declared names from a venue's own vocabulary (`MetaMask` aggregator ids).
    pub(crate) fn is_solver_name(&self, name: &str) -> bool {
        self.solver_names.contains(name)
    }

    /// Whether `address` is a batch-settlement venue (e.g. `CoW`). Such trades
    /// must be decoded by finding the order maker, not by tracking the
    /// sender: a solver settles many orders at once, so its net flow is not a
    /// single swap even when it happens to net to one token on each side.
    pub(crate) fn is_batch_settler(&self, address: Address) -> bool {
        self.batch_settlers.contains(&address)
    }

    pub(crate) fn wrapped_native(&self) -> Address {
        self.wrapped_native
    }

    /// Whether the address is token-movement infrastructure (Permit2, the wrapped-native
    /// token): contracts a trace touches on most swaps without ever being the settling venue,
    /// so attribution guesses must skip them.
    pub(crate) fn is_infrastructure(&self, address: Address) -> bool {
        self.infrastructure.contains(&address) || address == self.wrapped_native
    }

    /// The chain's `(stablecoin, decimals)` anchors for USD valuation.
    pub(crate) fn stablecoin_anchors(&self) -> &[(Address, u32)] {
        &self.usd_stablecoins
    }

    /// The named venue's address book section, when the book has one.
    pub(crate) fn venue(&self, name: &str) -> Option<&VenueAddresses> {
        self.venues.get(name)
    }

    /// The venue whose entry point this address is, if any.
    pub(crate) fn venue_name(&self, address: Address) -> Option<&str> {
        self.names
            .get(&address)
            .filter(|name| self.venues.contains_key(name.as_str()))
            .map(String::as_str)
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
        let registry = Registry::ethereum();
        assert!(!registry.solvers.is_empty());
        assert!(!registry
            .venue("relay")
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
    fn normalize_solver_trims_metamask_ids_and_passes_unknown_through() {
        let registry = Registry::ethereum();
        let metamask = registry.venue("metamask").unwrap();
        assert_eq!(metamask.normalize_solver("oneInchV6FeeDynamic"), "1inch");
        assert_eq!(metamask.normalize_solver("uniswapPermit2FeeDynamic"), "uniswap");
        assert_eq!(metamask.normalize_solver("okx6"), "okx");
        assert_eq!(metamask.normalize_solver("someFutureSolver"), "someFutureSolver");
    }

    #[test]
    fn solver_aliases_are_scoped_to_their_venue() {
        // The alias table is one venue's calldata vocabulary; a venue without one passes every
        // id through unchanged.
        let registry = Registry::ethereum();
        let relay = registry.venue("relay").unwrap();
        assert_eq!(relay.normalize_solver("oneInchV6FeeDynamic"), "oneInchV6FeeDynamic");
    }

    #[test]
    fn mixed_case_alias_needle_still_matches() {
        // Needles are canonicalized to lowercase at load, so a capitalized needle in the book
        // matches the same ids as a lowercase one.
        let book = ETHEREUM_TOML.replace(
            "[venues.metamask.solver_aliases]",
            "[venues.metamask.solver_aliases]\nBeBop = \"bebop\"",
        );
        let registry = Registry::from_toml(&book).unwrap();
        let metamask = registry.venue("metamask").unwrap();
        assert_eq!(metamask.normalize_solver("bebopJamV2"), "bebop");
    }

    #[test]
    fn infrastructure_covers_permit2_and_wrapped_native() {
        let registry = Registry::ethereum();
        let permit2 = address!("0x000000000022d473030f116ddee9f6b43ac78ba3");
        assert!(registry.is_infrastructure(permit2));
        assert!(registry.is_infrastructure(registry.wrapped_native()));
        assert!(!registry.is_infrastructure(addr(123)));
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
    fn venue_sections_resolve_by_name_and_entry_point() {
        let registry = Registry::ethereum();
        let relay = registry.venue("relay").unwrap();
        let collector = address!("0xf70da97812cb96acdf810712aa562db8dfa3dbef");
        let router = address!("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f");
        assert!(relay
            .fee_collectors
            .contains(&collector));
        assert!(!relay.fee_collectors.contains(&router));
        assert!(relay.entry_points.contains(&router));

        assert_eq!(registry.venue_name(router), Some("relay"));
        assert_eq!(
            registry.venue_name(address!("0x881d40237659c251811cec9c364ef91dc08d300c")),
            Some("metamask")
        );
        assert_eq!(registry.venue_name(collector), None);
        assert!(registry.venue("kyberswap").is_none());
    }

    #[test]
    fn venue_without_strategy_is_rejected() {
        // A venue section whose name has no decode strategy would silently never match, so the
        // book must fail to load.
        let text =
            format!("{ETHEREUM_TOML}\n[venues.reiay]\nentry_points = []\nfee_collectors = []\n");
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
        // known-venue Sender flow.
        let registry = Registry::ethereum();
        let rizzolver = address!("0x225a38bc71102999dd13478bfabd7c4d53f2dc17");
        assert_eq!(registry.label(rizzolver), "rizzolver");
        assert!(!registry.is_known(rizzolver));
        assert!(!registry.is_solver(rizzolver));
    }
}
