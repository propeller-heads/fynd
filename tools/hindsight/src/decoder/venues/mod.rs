//! Venue-specific decoders: the platforms users enter through (Relay, `MetaMask`).
//!
//! A venue owns the order flow — it picks a solver and may take a fee. One module here is one
//! venue, holding every decoder for it. A venue lists its decoders in its `decoders` constructor
//! (registered in `DECODERS`), tried in order: most are netting decoders (net the sender, back
//! the fee out, add venue-specific corrections), and a venue that is better read from its
//! calldata puts a calldata decoder ahead of or behind netting.
//!
//! Its address facts — entry points, fee collectors, solver aliases — are pure data in the
//! address book's `[venues.<name>]` section. The decoders are constructed with those addresses
//! when the registry loads, so each holds its own state.
//!
//! # What happens when a venue is missing
//!
//! Missing venue knowledge does not stop decoding — it degrades it, silently:
//!
//! - **Venue not in the address book at all**: its transactions only match when a known solver
//!   emitted a log inside them, and those decode via intent decoding, which excludes the sender —
//!   so most of the venue's trades are missed or declined. They surface as coverage gaps in
//!   `verify`, not as wrong records.
//! - **Venue registered but a fee collector is missing**: trades decode, but wrongly — the fee is
//!   not backed out, so the amounts include the venue's fee, and every comparison credits Fynd with
//!   money better routing cannot recover.
//!
//! The second failure mode is why fee collectors are verified against on-chain samples (see the
//! address book's comments) before a venue is added.

pub(crate) mod coinbase;
pub(crate) mod metamask;
pub(crate) mod rabby;
pub(crate) mod rainbow;
pub(crate) mod relay;

use crate::decoder::{decode::TradeDecoder, registry::VenueAddresses};

/// Constructs a venue's decoders from its address-book section, so each decoder holds the
/// addresses it needs as its own state.
type Constructor = fn(&VenueAddresses) -> Vec<Box<dyn TradeDecoder>>;

/// The one name → code binding: each address-book venue name maps to the constructor of its
/// decoders, in the order they are tried (first hit wins). Consulted once, when the registry
/// loads — the constructed decoders live on the venue's registry entry, and decoding calls them
/// as trait objects. Adding a venue is a `mod` declaration plus one row here; a `[venues.<name>]`
/// section with no row fails the registry load.
const DECODERS: &[(&str, Constructor)] = &[
    ("relay", relay::decoders),
    ("metamask", metamask::decoders),
    ("rabby", rabby::decoders),
    ("coinbase", coinbase::decoders),
    ("rainbow", rainbow::decoders),
];

/// Construct the named venue's decoders with its addresses, or `None` for a name with no
/// `DECODERS` row. Called by the registry at load time, once per venue.
pub(crate) fn build(name: &str, addresses: &VenueAddresses) -> Option<Vec<Box<dyn TradeDecoder>>> {
    DECODERS
        .iter()
        .find(|(registered, _)| *registered == name)
        .map(|(_, constructor)| constructor(addresses))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::registry::Registry;

    #[test]
    fn test_build_follows_the_registration_table() {
        // A registered venue constructs its decoders; an unknown name does not — which is what
        // lets the registry reject an address-book venue with no DECODERS row at load time.
        let registry = Registry::ethereum();
        let relay = registry.venue("relay").unwrap();
        assert!(!build("relay", relay)
            .unwrap()
            .is_empty());
        assert!(build("nope", relay).is_none());
    }
}
