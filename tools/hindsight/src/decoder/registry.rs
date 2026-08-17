//! The decoder's address book: which contracts are solvers, venues, batch settlers, and
//! venue infrastructure on one chain.
//!
//! The data is pure configuration and lives in TOML — the built-in address books are embedded
//! from the `registry/` directory; `--registry <path>` loads a modified or per-chain
//! address book without recompiling. This module only holds the lookups the decoders ask
//! (`Registry::is_solver`, `Registry::is_batch_settler`, `Registry::label`, …).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

use alloy::primitives::{Address, B256};
use anyhow::Context;
use serde::Deserialize;
use tycho_simulation::tycho_common::models::Chain;

/// The built-in address books, embedded at compile time (validated by tests, so they cannot fail
/// to parse at runtime).
const ETHEREUM_TOML: &str = include_str!("registry/ethereum.toml");
const BASE_TOML: &str = include_str!("registry/base.toml");
const UNICHAIN_TOML: &str = include_str!("registry/unichain.toml");
const ARBITRUM_TOML: &str = include_str!("registry/arbitrum.toml");
const BSC_TOML: &str = include_str!("registry/bsc.toml");
const POLYGON_TOML: &str = include_str!("registry/polygon.toml");
const ROBINHOOD_TOML: &str = include_str!("registry/robinhood.toml");

/// The chains that have a built-in address book, only for enumerating them: the tests cover every
/// book through this, and `Registry::builtin` names them when asked for a chain that has none.
/// The lookup itself is `builtin_book`, so no list is walked to resolve a chain.
const BUILTIN_CHAINS: [Chain; 7] = [
    Chain::Ethereum,
    Chain::Base,
    Chain::Unichain,
    Chain::Arbitrum,
    Chain::Bsc,
    Chain::Polygon,
    Chain::Robinhood,
];

/// The address book embedded for `chain`, or `None` for a chain Hindsight has none for.
///
/// Keyed on `Chain` rather than a chain name so the caller's string is parsed once, by
/// `fynd_core`'s parser, instead of being matched against spellings here. The wildcard arm is
/// forced: `Chain` is `#[non_exhaustive]`, so this cannot be an exhaustive match, which means a
/// chain Tycho adds later reaches it as "no built-in book" rather than as a compile error.
fn builtin_book(chain: Chain) -> Option<&'static str> {
    match chain {
        Chain::Ethereum => Some(ETHEREUM_TOML),
        Chain::Base => Some(BASE_TOML),
        Chain::Unichain => Some(UNICHAIN_TOML),
        Chain::Arbitrum => Some(ARBITRUM_TOML),
        Chain::Bsc => Some(BSC_TOML),
        Chain::Polygon => Some(POLYGON_TOML),
        Chain::Robinhood => Some(ROBINHOOD_TOML),
        _ => None,
    }
}

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
    /// Trader addresses that identify an order-flow venue (e.g. kpk's Safes on `CoW`). Absent in
    /// books that have no owner-identified venues.
    #[serde(default)]
    venue_owners: HashMap<Address, String>,
    /// Fee-wallet address → venue, for venues that route through a shared router and are only
    /// identified by the fee transferred to their wallet (Phantom, Robinhood). Absent in books
    /// with no fee-identified venues. Ordered so a trade cut by two venues' wallets resolves to
    /// the same venue on every run.
    #[serde(default)]
    venue_fees: BTreeMap<Address, String>,
    /// Provider integrator tag → venue, for venues identified by the integrator string in a
    /// provider's event (`LiFi` frontends: Infinex, Robinhood). Keys are lowercase. Absent in
    /// books with no integrator-identified venues.
    #[serde(default)]
    venue_integrators: HashMap<String, String>,
    /// `CoW` order `appData` hash → venue, for venues identified by the frontend tag (`appCode`)
    /// committed into an order (`LlamaSwap`). Absent in books with no appData-identified venues.
    #[serde(default)]
    venue_appdata: HashMap<B256, String>,
}

/// A venue's address-book section on one chain: the contracts users enter through, the
/// collectors its fees are sent to, and its calldata solver aliases. Pure data — a venue has no
/// code; decoding is per solver (see `crate::decoder::declared`), and the netting fallback reads
/// the collectors from here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VenueAddresses {
    pub(crate) entry_points: HashSet<Address>,
    pub(crate) fee_collectors: HashSet<Address>,
    /// Lowercase substrings of the venue's calldata solver ids, mapped to the solver name used
    /// in the address book. Ordered for deterministic matching; empty for venues that declare
    /// no solver in calldata.
    #[serde(default)]
    solver_aliases: BTreeMap<String, String>,
}

impl VenueAddresses {
    /// Normalize a solver id this venue declared in calldata to the address book's solver
    /// names: the first alias substring (in table order) contained in the lowercased id names
    /// the solver, trimming the venue's id decoration ("oneInchV6FeeDynamic" → "1inch") — not a
    /// 1:1 rename. Unmatched ids pass through as-is: still more informative than a raw executor
    /// address, and a signal to extend the address book.
    /// Whether this venue declares its solver in the entry calldata (it has alias entries to
    /// normalize the declared ids with).
    pub(crate) fn declares_solver(&self) -> bool {
        !self.solver_aliases.is_empty()
    }

    pub(crate) fn normalize_solver(&self, id: &str) -> String {
        let lower = id.to_lowercase();
        for (substring, name) in &self.solver_aliases {
            if lower.contains(substring) {
                return name.clone();
            }
        }
        id.to_string()
    }
}

/// A loaded solver entry: its display name joined with its `SolverDecoder` implementation.
/// Built once per address-book load; at trade time `Registry::solver` hands it out by address,
/// so no name is ever matched on a hot path.
pub(crate) struct Solver {
    pub(crate) name: String,
    /// The solver's decoder — the no-op implementation for book-only solvers.
    pub(crate) decoder: &'static dyn crate::decoder::solvers::SolverDecoder,
}

impl std::fmt::Debug for Solver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Solver")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Per-chain address book for trade decoding, loaded from TOML (see the module docs).
#[derive(Debug)]
pub(crate) struct Registry {
    /// Solver routers — the entries that actually settle a swap, each carrying its decoder.
    solvers: HashMap<Address, Solver>,
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
    /// keep matching via their solver logs (Intent), not sender netting.
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
    /// Venue address sets, keyed by the venue name from the address book.
    venues: HashMap<String, VenueAddresses>,
    /// Trader address → order-flow venue name, for venues identified by who owns the order
    /// rather than by the entry point (e.g. kpk's Safes settling through `CoW`).
    venue_owners: HashMap<Address, String>,
    /// Fee-wallet address → venue name, for venues identified by the fee they take on a shared
    /// router rather than by the entry point (Phantom, Robinhood). Ordered for deterministic
    /// attribution when two venues' wallets both take a cut of one trade.
    venue_fees: BTreeMap<Address, String>,
    /// Provider integrator tag (lowercase) → venue name, for venues identified by the integrator
    /// string a provider records in its event (`LiFi` frontends: Infinex, Robinhood).
    venue_integrators: HashMap<String, String>,
    /// `CoW` order `appData` hash → venue name, for venues identified by the frontend tag
    /// (`appCode`) committed into an order (`LlamaSwap`).
    venue_appdata: HashMap<B256, String>,
}

impl Registry {
    /// The address book embedded for `chain`.
    ///
    /// # Errors
    /// When Hindsight has no book for that chain; the caller can still supply one with
    /// `Registry::from_file`.
    pub(crate) fn builtin(chain: Chain) -> anyhow::Result<Self> {
        let Some(text) = builtin_book(chain) else {
            let known = BUILTIN_CHAINS
                .map(|builtin| builtin.to_string())
                .join(", ");
            anyhow::bail!(
                "no built-in decoder address registry for chain '{chain}' (only {known}); \
                 pass --registry with that chain's address book"
            );
        };
        Self::from_toml(text).with_context(|| format!("invalid built-in {chain} address book"))
    }

    /// The address book in an explicit TOML file. Takes no chain: the file says what it describes,
    /// which is what lets a chain with no built-in book — or none Tycho knows — be decoded at all.
    pub(crate) fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read registry file {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("invalid registry file {}", path.display()))
    }

    /// The built-in Ethereum address book — the fixture the tests decode against. Production code
    /// goes through `builtin`, which resolves the book from the parsed `--chain`.
    #[cfg(test)]
    pub(crate) fn ethereum() -> Self {
        Self::from_toml(ETHEREUM_TOML).expect("embedded ethereum registry must parse")
    }

    fn from_toml(text: &str) -> anyhow::Result<Self> {
        let mut book: AddressBook =
            toml::from_str(text).context("failed to parse address book TOML")?;

        let mut names = book.solvers.clone();
        for (name, venue) in &book.venues {
            for &entry_point in &venue.entry_points {
                names.insert(entry_point, name.clone());
            }
        }
        let solver_names = book.solvers.values().cloned().collect();
        let solvers = book
            .solvers
            .into_iter()
            .map(|(address, name)| {
                let decoder = crate::decoder::solvers::decoder_for(&name);
                (address, Solver { name, decoder })
            })
            .collect();
        let mut usd_stablecoins: Vec<(Address, u32)> = book
            .usd_stablecoins
            .into_iter()
            .collect();
        usd_stablecoins.sort_unstable();
        // Alias substrings match against lowercased ids, so a mixed-case entry in the address
        // book would silently never match — canonicalize at load.
        for venue in book.venues.values_mut() {
            venue.solver_aliases = std::mem::take(&mut venue.solver_aliases)
                .into_iter()
                .map(|(substring, name)| (substring.to_lowercase(), name))
                .collect();
        }

        Ok(Self {
            solver_names,
            solvers,
            names,
            batch_settlers: book.batch_settlers,
            labels: book.labels,
            wrapped_native: book.wrapped_native,
            infrastructure: book.infrastructure,
            usd_stablecoins,
            venues: book.venues,
            venue_owners: book.venue_owners,
            venue_fees: book.venue_fees,
            venue_integrators: book
                .venue_integrators
                .into_iter()
                .map(|(tag, venue)| (tag.to_lowercase(), venue))
                .collect(),
            venue_appdata: book.venue_appdata,
        })
    }

    /// Whether the address is a known venue or solver.
    pub(crate) fn is_known(&self, address: Address) -> bool {
        self.names.contains_key(&address)
    }

    pub(crate) fn is_solver(&self, address: Address) -> bool {
        self.solvers.contains_key(&address)
    }

    /// The loaded solver entry for a router address — name and decoder — if the address book has
    /// one.
    pub(crate) fn solver(&self, address: Address) -> Option<&Solver> {
        self.solvers.get(&address)
    }

    /// Whether `name` is a registered solver's display name. Bounds the metric label
    /// names: attribution can also produce raw addresses, venue names (fallback tier), and
    /// names a venue declared in calldata (`MetaMask` aggregator ids).
    pub(crate) fn is_solver_name(&self, name: &str) -> bool {
        self.solver_names.contains(name)
    }

    /// Whether `address` is a batch-settlement venue (e.g. `CoW`). Such trades
    /// must be decoded by finding the swapper, not by tracking the
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

    /// Whether the address is a registered fee collector: a venue section's collector or a
    /// fee-identified venue wallet. Fees paid to these are venue fees — backed out by the venue
    /// decoders and `venue_attribution` — not token-level transfer fees (see
    /// `veto::Veto::FeeOnTransfer`).
    pub(crate) fn is_fee_collector(&self, address: Address) -> bool {
        self.venue_fees.contains_key(&address) ||
            self.venues
                .values()
                .any(|venue| venue.fee_collectors.contains(&address))
    }

    /// The chain's `(stablecoin, decimals)` anchors for USD valuation.
    pub(crate) fn stablecoin_anchors(&self) -> &[(Address, u32)] {
        &self.usd_stablecoins
    }

    /// The named venue's address book section, when the book has one.
    pub(crate) fn venue(&self, name: &str) -> Option<&VenueAddresses> {
        self.venues.get(name)
    }

    /// The order-flow venue that owns trades from this trader address, if any. Used to attribute
    /// venues identified by who owns the order (e.g. kpk's Safes) rather than by `tx.to`.
    pub(crate) fn venue_for_owner(&self, address: Address) -> Option<&str> {
        self.venue_owners
            .get(&address)
            .map(String::as_str)
    }

    /// Fee-wallet → venue map, for attributing venues identified only by their fee leg on a
    /// shared router (see `crate::decoder::attribution`), in address order.
    pub(crate) fn venue_fees(&self) -> &BTreeMap<Address, String> {
        &self.venue_fees
    }

    /// The venue for a provider integrator tag (case-insensitive), if one is registered. Used to
    /// attribute `LiFi` frontends by the integrator string in the swap event.
    pub(crate) fn venue_for_integrator(&self, integrator: &str) -> Option<&str> {
        self.venue_integrators
            .get(&integrator.to_lowercase())
            .map(String::as_str)
    }

    /// The venue for a `CoW` order `appData` hash, if one is registered. Used to attribute
    /// frontends by the `appCode` tag committed into the order (`LlamaSwap`).
    pub(crate) fn venue_for_appdata(&self, app_data: B256) -> Option<&str> {
        self.venue_appdata
            .get(&app_data)
            .map(String::as_str)
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
    fn test_every_embedded_book_parses() {
        // Every book is embedded and loaded by name, so a malformed one is a runtime panic in
        // `load`. Parse them all here, and require the parts every chain must have: a wrapped
        // native token, a USD anchor, solvers, and Relay (the one venue present on all of them).
        for chain in BUILTIN_CHAINS {
            // `load` names the chain in its error context, so unwrapping reports which book broke.
            let registry = Registry::builtin(chain).unwrap();
            assert!(!registry.wrapped_native.is_zero(), "{chain} has no wrapped native token");
            assert!(!registry.usd_stablecoins.is_empty(), "{chain} has no USD anchor");
            assert!(!registry.solvers.is_empty(), "{chain} has no solvers");
            assert!(
                !registry
                    .venue("relay")
                    .unwrap()
                    .entry_points
                    .is_empty(),
                "{chain} has no relay entry points"
            );
        }
    }

    #[test]
    fn test_builtin_chains_agrees_with_the_lookup() {
        // BUILTIN_CHAINS only enumerates — for the error message and for the book coverage above —
        // while `builtin_book` decides. Drift between them would drop a chain from the error and
        // silently leave its book untested, so every chain Tycho names today is checked against
        // both. `Chain` is non_exhaustive, hence the explicit list rather than a variant sweep.
        let tycho_chains = [
            Chain::Ethereum,
            Chain::Starknet,
            Chain::ZkSync,
            Chain::Arbitrum,
            Chain::Base,
            Chain::Bsc,
            Chain::Unichain,
            Chain::Polygon,
            Chain::Plasma,
            Chain::Robinhood,
        ];
        for chain in tycho_chains {
            assert_eq!(
                builtin_book(chain).is_some(),
                BUILTIN_CHAINS.contains(&chain),
                "{chain} is in one of BUILTIN_CHAINS / builtin_book but not the other"
            );
        }
    }

    #[test]
    fn test_builtin_book_is_keyed_on_the_chain_not_its_name() {
        // Every supported chain resolves without any spelling of its name being involved.
        for chain in BUILTIN_CHAINS {
            assert!(builtin_book(chain).is_some(), "{chain} has no book");
        }
        // A chain Tycho knows but Hindsight has no book for falls through the wildcard arm.
        assert!(builtin_book(Chain::Plasma).is_none());
        assert!(builtin_book(Chain::Starknet).is_none());
    }

    #[test]
    fn test_load_unknown_chain_lists_the_builtin_ones() {
        let error = Registry::builtin(Chain::Plasma)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no built-in decoder address registry"), "{error}");
        assert!(error.contains("polygon"), "the error must list the built-in chains: {error}");
    }

    #[test]
    fn test_tycho_router_is_a_solver_on_every_chain() {
        // Hindsight compares Fynd against what settled, so the chain's own Tycho router must be
        // recognised — otherwise Fynd's own settled trades never match.
        for chain in BUILTIN_CHAINS {
            let registry = Registry::builtin(chain).unwrap();
            assert!(
                registry
                    .solvers
                    .values()
                    .any(|solver| solver.name == "tycho"),
                "{chain} has no tycho router"
            );
        }
    }

    #[test]
    fn test_load_unreadable_and_invalid_files() {
        let missing = Registry::from_file(Path::new("/nonexistent/book.toml"));
        assert!(missing
            .unwrap_err()
            .to_string()
            .contains("failed to read registry file"));

        let dir = std::env::temp_dir().join("hindsight_registry_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("invalid.toml");
        std::fs::write(&path, "wrapped_native = \"not an address\"").unwrap();
        let invalid = Registry::from_file(&path);
        assert!(invalid
            .unwrap_err()
            .to_string()
            .contains("invalid registry file"));
    }

    #[test]
    fn test_unknown_toml_key() {
        // A typo'd section must fail loudly, not silently drop addresses.
        let text = format!("{ETHEREUM_TOML}\n[solverz]\n");
        assert!(Registry::from_toml(&text).is_err());
    }

    #[test]
    fn test_registries_named() {
        let registry = Registry::ethereum();
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        assert!(registry.is_solver(oneinch));
        assert!(registry.is_known(relay));
        assert!(!registry.is_solver(relay));
    }

    #[test]
    fn test_normalize_solver_metamask_and_unknown_ids() {
        let registry = Registry::ethereum();
        let metamask = registry.venue("metamask").unwrap();
        assert_eq!(metamask.normalize_solver("oneInchV6FeeDynamic"), "1inch");
        assert_eq!(metamask.normalize_solver("uniswapPermit2FeeDynamic"), "uniswap");
        assert_eq!(metamask.normalize_solver("okx6"), "okx");
        assert_eq!(metamask.normalize_solver("someFutureSolver"), "someFutureSolver");
    }

    #[test]
    fn test_solver_alias_venue_scoping() {
        // The alias table is one venue's calldata names; a venue without one passes every
        // id through unchanged.
        let registry = Registry::ethereum();
        let relay = registry.venue("relay").unwrap();
        assert_eq!(relay.normalize_solver("oneInchV6FeeDynamic"), "oneInchV6FeeDynamic");
    }

    #[test]
    fn test_mixed_case_alias_substring() {
        // Alias substrings are canonicalized to lowercase at load, so a capitalized entry in the
        // address book matches the same ids as a lowercase one.
        let book = ETHEREUM_TOML.replace(
            "[venues.metamask.solver_aliases]",
            "[venues.metamask.solver_aliases]\nBeBop = \"bebop\"",
        );
        let registry = Registry::from_toml(&book).unwrap();
        let metamask = registry.venue("metamask").unwrap();
        assert_eq!(metamask.normalize_solver("bebopJamV2"), "bebop");
    }

    #[test]
    fn test_infrastructure_permit2_and_wrapped_native() {
        let registry = Registry::ethereum();
        let permit2 = address!("0x000000000022d473030f116ddee9f6b43ac78ba3");
        assert!(registry.is_infrastructure(permit2));
        assert!(registry.is_infrastructure(registry.wrapped_native()));
        assert!(!registry.is_infrastructure(addr(123)));
    }

    #[test]
    fn test_batch_settler_lookup() {
        let registry = Registry::ethereum();
        let cow = address!("0x9008d19f58aabd9ed0d60971565aa8510560ab41");
        let oneinch = address!("0x111111125421ca6dc452d289314280a0f8842a65");
        assert!(registry.is_batch_settler(cow));
        assert!(!registry.is_batch_settler(oneinch));
        assert!(!registry.is_batch_settler(addr(123)));
    }

    #[test]
    fn test_venue_section_lookup_by_name_and_entry_point() {
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
    fn test_label_known_and_unknown() {
        let registry = Registry::ethereum();
        let relay = address!("0xf5042e6ffac5a625d4e7848e0b01373d8eb9e222");
        let unknown = addr(123);
        assert_eq!(registry.label(relay), "relay");
        assert_eq!(registry.label(unknown), unknown.to_string());
    }

    #[test]
    fn test_display_label_resolution() {
        // Solver/filler labels are display-only: is_known drives strategy selection, and a
        // filler-entered tx must keep matching via its solver logs (Intent), not as a
        // known-venue Sender flow.
        let registry = Registry::ethereum();
        let rizzolver = address!("0x225a38bc71102999dd13478bfabd7c4d53f2dc17");
        assert_eq!(registry.label(rizzolver), "rizzolver");
        assert!(!registry.is_known(rizzolver));
        assert!(!registry.is_solver(rizzolver));
    }
}
