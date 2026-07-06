//! Cache key derivation for the quote cache.
//!
//! The key identifies a cacheable quote request. It is derived from the request [`Order`] through
//! a [`KeyNormalizer`] so ENG-6234's gating verification can later drop fields (e.g. re-address
//! any caller) or add amount bucketing without touching the cache internals.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::models::Address;

use crate::{Order, OrderSide};

/// Identity of a cacheable quote request.
///
/// Derived from the request [`Order`], never from `order.id` (server-generated per request, so it
/// would make every request a cache miss). `sender`/`receiver` are included for now: the cached
/// solve is re-encoded on a hit, and keeping them in the key keeps that re-encode addressed to the
/// exact caller. ENG-6234 may drop them once verified safe — swap the [`KeyNormalizer`] to do so.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QuoteCacheKey {
    token_in: Address,
    token_out: Address,
    amount: BigUint,
    side: OrderSide,
    sender: Address,
    receiver: Address,
}

/// Derives a [`QuoteCacheKey`] from an [`Order`].
///
/// Behind a trait so the key surface can change (field drop, amount bucketing) without reworking
/// the cache — ENG-6234 provides the verified normalizer.
pub trait KeyNormalizer: Send + Sync {
    /// Maps an order to its cache key.
    fn normalize(&self, order: &Order) -> QuoteCacheKey;
}

/// The identity normalizer: every distinguishing order field (except the server-generated id)
/// becomes part of the key, including `sender`/`receiver`.
pub struct IdentityNormalizer;

impl KeyNormalizer for IdentityNormalizer {
    fn normalize(&self, order: &Order) -> QuoteCacheKey {
        QuoteCacheKey {
            token_in: order.token_in().clone(),
            token_out: order.token_out().clone(),
            amount: order.amount().clone(),
            side: order.side(),
            sender: order.sender().clone(),
            // effective_receiver falls back to sender, matching how the order is actually solved.
            receiver: order.effective_receiver(),
        }
    }
}
