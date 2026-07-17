//! Venue-specific decoders: the platforms users enter through (Relay, `MetaMask`).
//!
//! A venue owns the order flow — it picks a solver and may take a fee. One module here is one
//! venue, holding every decoder for it. A venue lists its decoders in `decoders_for`, tried in
//! order: today each is a netting decoder (net the sender, back the fee out, add venue-specific
//! corrections), and a venue that is better read from its calldata would add a calldata decoder
//! ahead of or behind netting.
//!
//! Its address facts — entry points, fee collectors, solver aliases — are pure data in the
//! address book's `[venues.<name>]` section, handed to the decoder through the context.
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

pub(crate) mod metamask;
pub(crate) mod rabby;
pub(crate) mod relay;

use alloy::providers::{Provider, RootProvider};

use crate::decoder::decode::TradeDecoder;

/// The decoders tried for a venue, in order (first hit wins). This is the one place a venue is
/// registered — adding a venue is a `mod` declaration plus one arm here. A name that resolves to
/// no decoders is rejected by the registry at load time (see `has_decoder`).
pub(crate) fn decoders_for<P: Provider>(name: &str) -> Vec<Box<dyn TradeDecoder<P>>> {
    match name {
        "relay" => vec![Box::new(relay::RelayNetting)],
        "metamask" => vec![Box::new(metamask::MetaMaskNetting)],
        "rabby" => vec![Box::new(rabby::RabbyNetting)],
        _ => vec![],
    }
}

/// Whether a venue name resolves to a decoder — derived from `decoders_for` so the two cannot
/// drift. The registry uses this at load time to reject an address-book venue with no decoder.
/// The provider type is irrelevant; only whether a decoder exists matters.
pub(crate) fn has_decoder(name: &str) -> bool {
    !decoders_for::<RootProvider>(name).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_decoder_follows_decoders_for() {
        // A registered venue resolves; an unknown name does not. Adding a venue needs no change
        // here — `has_decoder` derives from the one `decoders_for` registration.
        assert!(has_decoder("relay"));
        assert!(!has_decoder("nope"));
    }
}
