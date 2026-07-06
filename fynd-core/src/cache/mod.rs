//! In-process cache of recently solved quotes (ENG-6236, TECH_DOC §3 Phase B).
//!
//! On a repeat quote request for the same market pair/amount/caller, the handler serves the cached
//! pre-encode solve and re-encodes fresh calldata instead of running the solver — Relay's
//! quote→execute pattern at encode-only latency. The cached [`OrderQuote`] carries gas already
//! refined by [`solve`](crate::WorkerPoolRouter::solve); re-encoding recomputes the transaction,
//! gas estimate, and fee breakdown for the caller's options, so a hit is equivalent to a fresh
//! solve at the same block.
//!
//! Entries are in-memory only: an [`OrderQuote`]'s route holds live `protocol_state` trait objects
//! that are deliberately never serialized.
//!
//! ## Concurrency
//!
//! The cache is shared across all actix workers behind a single [`std::sync::Mutex`]. Quote traffic
//! is far below the rate at which a short critical section (a hash lookup plus, on a hit, one
//! `OrderQuote` clone) would contend, so a global lock is simpler and adequate — no sharding. Every
//! method locks, mutates or reads, and returns before any `.await`; encoding of a hit happens in
//! the handler after the returned clone, never under the lock (clippy `await_holding_lock` is
//! denied).

/// Cache key derivation ([`QuoteCacheKey`], [`KeyNormalizer`]).
pub mod key;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use metrics::{counter, gauge};

use crate::{
    cache::key::{IdentityNormalizer, KeyNormalizer, QuoteCacheKey},
    Order, OrderQuote, QuoteRequest,
};

/// Identity used when the auth proxy sets no `User-Identity` header (e.g. self-hosted Fynd): all
/// such requests share one bucket for the per-identity cap.
pub const ANONYMOUS_IDENTITY: &str = "__anonymous__";

/// Tunable limits for the quote cache. Defaults match TECH_DOC §3 B2; all overridable.
#[derive(Clone, Debug)]
pub struct QuoteCachePolicy {
    /// Sliding time-to-live measured from the last request that touched an entry.
    pub ttl: Duration,
    /// Maximum entries across all identities. Excess evicts the global LRU entry.
    pub global_cap: usize,
    /// Maximum entries per identity. Excess evicts the LRU entry within that identity.
    pub per_identity_cap: usize,
    /// An entry solved more than this many blocks before the current head is treated as a miss.
    pub staleness_blocks: u64,
}

impl Default for QuoteCachePolicy {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            global_cap: 500,
            per_identity_cap: 50,
            staleness_blocks: 3,
        }
    }
}

/// A cached pre-encode solve plus the metadata the cache and the ENG-6237 refresher need.
#[derive(Clone)]
pub struct CacheEntry {
    /// The winning pre-encode order quote (gas already refined by `solve`).
    solved: OrderQuote,
    /// The originating request, kept so the ENG-6237 refresher can re-solve this entry each block.
    #[allow(dead_code)] // Consumed by the ENG-6237 refresh scheduler.
    request: QuoteRequest,
    /// Block the solve was computed against, for the staleness cutoff.
    solved_at_block: u64,
    /// Last request that touched this entry, for the sliding TTL and LRU eviction.
    last_requested: Instant,
    /// Identity that owns this entry for the per-identity cap.
    api_key_identity: String,
}

impl CacheEntry {
    /// Returns the cached pre-encode order quote.
    pub fn solved(&self) -> &OrderQuote {
        &self.solved
    }

    /// Returns the originating request (used by the ENG-6237 refresher to re-solve).
    #[allow(dead_code)] // Consumed by the ENG-6237 refresh scheduler.
    pub fn request(&self) -> &QuoteRequest {
        &self.request
    }

    /// Returns the block the solve was computed against.
    pub fn solved_at_block(&self) -> u64 {
        self.solved_at_block
    }

    /// Returns the identity that owns this entry.
    pub fn api_key_identity(&self) -> &str {
        &self.api_key_identity
    }
}

/// Wall-clock source, abstracted so tests can drive TTL and LRU deterministically.
trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock backed by [`Instant::now`].
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Locked interior: the entry map plus a per-identity index for cap enforcement and LRU eviction.
struct CacheInner {
    entries: HashMap<QuoteCacheKey, CacheEntry>,
    by_identity: HashMap<String, HashSet<QuoteCacheKey>>,
}

/// Shared cache of recently solved quotes.
pub struct QuoteCache {
    inner: Mutex<CacheInner>,
    policy: QuoteCachePolicy,
    normalizer: Box<dyn KeyNormalizer>,
    clock: Arc<dyn Clock>,
}

impl QuoteCache {
    /// Creates a cache with the given policy, the identity key normalizer, and the system clock.
    pub fn new(policy: QuoteCachePolicy) -> Self {
        Self {
            inner: Mutex::new(CacheInner { entries: HashMap::new(), by_identity: HashMap::new() }),
            policy,
            normalizer: Box::new(IdentityNormalizer),
            clock: Arc::new(SystemClock),
        }
    }

    /// Looks up a cached solve for `order` against the current chain head.
    ///
    /// Returns a clone of the cached pre-encode [`OrderQuote`] on a hit (the caller re-stamps
    /// sender/receiver and encodes). A hit slides the TTL. An entry that is expired (TTL) or stale
    /// (older than `current_block - staleness_blocks`) is dropped and reported as a miss. Emits the
    /// hit/miss counters and the entries gauge.
    pub fn get(&self, order: &Order, current_block: u64) -> Option<OrderQuote> {
        let key = self.normalizer.normalize(order);
        let now = self.clock.now();
        let mut inner = self
            .inner
            .lock()
            .expect("quote cache mutex poisoned");

        let Some(entry) = inner.entries.get(&key) else {
            counter!("quote_cache_misses_total").increment(1);
            return None;
        };

        let expired = now.duration_since(entry.last_requested) > self.policy.ttl;
        let stale =
            current_block.saturating_sub(entry.solved_at_block) > self.policy.staleness_blocks;
        if expired || stale {
            remove_key(&mut inner, &key);
            set_entries_gauge(&inner);
            counter!("quote_cache_misses_total").increment(1);
            return None;
        }

        let entry = inner
            .entries
            .get_mut(&key)
            .expect("entry present: checked above under the same lock");
        entry.last_requested = now;
        let solved = entry.solved.clone();
        counter!("quote_cache_hits_total").increment(1);
        Some(solved)
    }

    /// Inserts (or replaces) the solve for `order`, registering it under `identity`.
    ///
    /// Enforces the per-identity cap first (evict LRU within the identity), then the global cap
    /// (evict global LRU). Also registers the request so the ENG-6237 refresher can re-solve hot
    /// entries. Emits the eviction counter and the entries gauge.
    pub fn insert(
        &self,
        order: &Order,
        solved: OrderQuote,
        request: QuoteRequest,
        identity: &str,
        solved_at_block: u64,
    ) {
        let key = self.normalizer.normalize(order);
        let now = self.clock.now();
        let mut inner = self
            .inner
            .lock()
            .expect("quote cache mutex poisoned");

        // Drop any prior entry for this key so a changed owning identity leaves no stale index
        // slot.
        remove_key(&mut inner, &key);

        inner.entries.insert(
            key.clone(),
            CacheEntry {
                solved,
                request,
                solved_at_block,
                last_requested: now,
                api_key_identity: identity.to_string(),
            },
        );
        inner
            .by_identity
            .entry(identity.to_string())
            .or_default()
            .insert(key);

        self.evict_over_identity_cap(&mut inner, identity);
        self.evict_over_global_cap(&mut inner);
        set_entries_gauge(&inner);
    }

    /// Evicts the LRU entries within `identity` until it is within the per-identity cap.
    fn evict_over_identity_cap(&self, inner: &mut CacheInner, identity: &str) {
        loop {
            let over_cap = inner
                .by_identity
                .get(identity)
                .is_some_and(|keys| keys.len() > self.policy.per_identity_cap);
            if !over_cap {
                break;
            }
            let victim = inner
                .by_identity
                .get(identity)
                .and_then(|keys| lru_key(inner, keys.iter()));
            match victim {
                Some(key) => {
                    remove_key(inner, &key);
                    counter!("quote_cache_evictions_total").increment(1);
                }
                None => break,
            }
        }
    }

    /// Evicts the global LRU entries until the total is within the global cap.
    fn evict_over_global_cap(&self, inner: &mut CacheInner) {
        while inner.entries.len() > self.policy.global_cap {
            let victim = lru_key(inner, inner.entries.keys());
            match victim {
                Some(key) => {
                    remove_key(inner, &key);
                    counter!("quote_cache_evictions_total").increment(1);
                }
                None => break,
            }
        }
    }

    /// Snapshots every live entry with its key, for the ENG-6237 refresher to iterate and re-solve.
    #[allow(dead_code)] // Consumed by the ENG-6237 refresh scheduler.
    pub fn snapshot(&self) -> Vec<(QuoteCacheKey, CacheEntry)> {
        let inner = self
            .inner
            .lock()
            .expect("quote cache mutex poisoned");
        inner
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect()
    }

    /// Number of live entries. Test/introspection helper.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("quote cache mutex poisoned")
            .entries
            .len()
    }

    /// Overrides the clock (tests only) so TTL and LRU can be driven deterministically.
    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

/// Returns the least-recently-requested key among `candidates`, or `None` if empty.
fn lru_key<'a>(
    inner: &CacheInner,
    candidates: impl Iterator<Item = &'a QuoteCacheKey>,
) -> Option<QuoteCacheKey> {
    let mut oldest: Option<(&QuoteCacheKey, Instant)> = None;
    for key in candidates {
        let Some(entry) = inner.entries.get(key) else { continue };
        let is_older = match oldest {
            Some((_, ts)) => entry.last_requested < ts,
            None => true,
        };
        if is_older {
            oldest = Some((key, entry.last_requested));
        }
    }
    oldest.map(|(key, _)| key.clone())
}

/// Removes a key from the entry map and from its owning identity index, pruning an emptied bucket.
fn remove_key(inner: &mut CacheInner, key: &QuoteCacheKey) -> Option<CacheEntry> {
    let entry = inner.entries.remove(key)?;
    if let Some(keys) = inner
        .by_identity
        .get_mut(&entry.api_key_identity)
    {
        keys.remove(key);
        if keys.is_empty() {
            inner
                .by_identity
                .remove(&entry.api_key_identity);
        }
    }
    Some(entry)
}

/// Publishes the live entry count to the metrics gauge.
fn set_entries_gauge(inner: &CacheInner) {
    gauge!("quote_cache_entries").set(inner.entries.len() as f64);
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_simulation::tycho_common::{models::Address, Bytes};

    use super::*;
    use crate::{BlockInfo, OrderSide, QuoteOptions, QuoteStatus};

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn order(sender: u8, amount: u64) -> Order {
        Order::new(addr(0x01), addr(0x02), BigUint::from(amount), OrderSide::Sell, addr(sender))
    }

    fn solved(block: u64) -> OrderQuote {
        OrderQuote::new(
            "order-id".to_string(),
            QuoteStatus::Success,
            BigUint::from(1000u64),
            BigUint::from(990u64),
            BigUint::from(100u64),
            BigUint::from(900u64),
            BlockInfo::new(block, "0xhash".to_string(), block),
            "algo".to_string(),
            Bytes::from(addr(0x01).as_ref()),
            Bytes::from(addr(0x01).as_ref()),
            "1".to_string(),
        )
    }

    fn request(order: Order) -> QuoteRequest {
        QuoteRequest::new(vec![order], QuoteOptions::default())
    }

    /// Test clock with a settable instant.
    struct ManualClock {
        now: Mutex<Instant>,
    }

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self { now: Mutex::new(Instant::now()) })
        }

        fn advance(&self, delta: Duration) {
            let mut now = self.now.lock().unwrap();
            *now += delta;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
    }

    #[test]
    fn key_excludes_order_id() {
        let normalizer = IdentityNormalizer;
        let a = order(0xAA, 1000).with_id("id-a".to_string());
        let b = order(0xAA, 1000).with_id("id-b".to_string());
        assert_eq!(normalizer.normalize(&a), normalizer.normalize(&b));
    }

    #[test]
    fn key_includes_sender() {
        let normalizer = IdentityNormalizer;
        assert_ne!(
            normalizer.normalize(&order(0xAA, 1000)),
            normalizer.normalize(&order(0xBB, 1000))
        );
    }

    #[test]
    fn key_includes_amount() {
        let normalizer = IdentityNormalizer;
        assert_ne!(
            normalizer.normalize(&order(0xAA, 1000)),
            normalizer.normalize(&order(0xAA, 2000))
        );
    }

    #[test]
    fn hit_returns_cached_solve() {
        let cache = QuoteCache::new(QuoteCachePolicy::default());
        let order = order(0xAA, 1000);
        cache.insert(&order, solved(100), request(order.clone()), "id-1", 100);
        assert!(cache.get(&order, 100).is_some());
    }

    #[test]
    fn miss_on_absent_key() {
        let cache = QuoteCache::new(QuoteCachePolicy::default());
        assert!(cache
            .get(&order(0xAA, 1000), 100)
            .is_none());
    }

    #[test]
    fn ttl_expiry_is_miss_and_drops_entry() {
        let clock = ManualClock::new();
        let policy = QuoteCachePolicy { ttl: Duration::from_secs(300), ..Default::default() };
        let cache = QuoteCache::new(policy).with_clock(clock.clone());
        let order = order(0xAA, 1000);
        cache.insert(&order, solved(100), request(order.clone()), "id-1", 100);

        clock.advance(Duration::from_secs(301));
        assert!(cache.get(&order, 100).is_none());
        assert_eq!(cache.len(), 0, "expired entry should be dropped");
    }

    #[test]
    fn get_slides_ttl() {
        let clock = ManualClock::new();
        let policy = QuoteCachePolicy { ttl: Duration::from_secs(300), ..Default::default() };
        let cache = QuoteCache::new(policy).with_clock(clock.clone());
        let order = order(0xAA, 1000);
        cache.insert(&order, solved(100), request(order.clone()), "id-1", 100);

        // Touch just before expiry, then advance again by less than the TTL: still a hit.
        clock.advance(Duration::from_secs(200));
        assert!(cache.get(&order, 100).is_some());
        clock.advance(Duration::from_secs(200));
        assert!(
            cache.get(&order, 100).is_some(),
            "sliding TTL should keep a recently touched entry"
        );
    }

    #[test]
    fn staleness_cutoff_is_miss() {
        let policy = QuoteCachePolicy { staleness_blocks: 3, ..Default::default() };
        let cache = QuoteCache::new(policy);
        let order = order(0xAA, 1000);
        cache.insert(&order, solved(100), request(order.clone()), "id-1", 100);

        // Head advanced by exactly the cutoff: still fresh.
        assert!(cache.get(&order, 103).is_some());
        // One block past the cutoff: miss, entry dropped.
        cache.insert(&order, solved(100), request(order.clone()), "id-1", 100);
        assert!(cache.get(&order, 104).is_none());
    }

    #[test]
    fn per_identity_cap_evicts_within_identity() {
        let policy = QuoteCachePolicy { per_identity_cap: 2, ..Default::default() };
        let clock = ManualClock::new();
        let cache = QuoteCache::new(policy).with_clock(clock.clone());

        // Three entries for one identity; each insert a tick apart so LRU order is well-defined.
        let first = order(0xAA, 1000);
        cache.insert(&first, solved(100), request(first.clone()), "id-1", 100);
        clock.advance(Duration::from_secs(1));
        let second = order(0xAA, 2000);
        cache.insert(&second, solved(100), request(second.clone()), "id-1", 100);
        clock.advance(Duration::from_secs(1));
        let third = order(0xAA, 3000);
        cache.insert(&third, solved(100), request(third.clone()), "id-1", 100);

        assert_eq!(cache.len(), 2, "per-identity cap holds");
        assert!(cache.get(&first, 100).is_none(), "LRU entry evicted");
        assert!(cache.get(&third, 100).is_some());
    }

    #[test]
    fn per_identity_cap_is_isolated() {
        let policy = QuoteCachePolicy { per_identity_cap: 1, ..Default::default() };
        let cache = QuoteCache::new(policy);
        let a = order(0xAA, 1000);
        let b = order(0xBB, 1000);
        cache.insert(&a, solved(100), request(a.clone()), "id-1", 100);
        cache.insert(&b, solved(100), request(b.clone()), "id-2", 100);
        // Different identities each keep their own entry despite the cap of 1 per identity.
        assert!(cache.get(&a, 100).is_some());
        assert!(cache.get(&b, 100).is_some());
    }

    #[test]
    fn global_cap_evicts_global_lru() {
        let policy = QuoteCachePolicy { global_cap: 2, per_identity_cap: 50, ..Default::default() };
        let clock = ManualClock::new();
        let cache = QuoteCache::new(policy).with_clock(clock.clone());

        // One entry per identity so only the global cap can bite.
        let a = order(0xAA, 1000);
        cache.insert(&a, solved(100), request(a.clone()), "id-1", 100);
        clock.advance(Duration::from_secs(1));
        let b = order(0xBB, 1000);
        cache.insert(&b, solved(100), request(b.clone()), "id-2", 100);
        clock.advance(Duration::from_secs(1));
        let c = order(0xCC, 1000);
        cache.insert(&c, solved(100), request(c.clone()), "id-3", 100);

        assert_eq!(cache.len(), 2, "global cap holds");
        assert!(cache.get(&a, 100).is_none(), "global LRU evicted");
        assert!(cache.get(&c, 100).is_some());
    }

    #[test]
    fn snapshot_exposes_stored_request() {
        let cache = QuoteCache::new(QuoteCachePolicy::default());
        let order = order(0xAA, 1000);
        cache.insert(&order, solved(100), request(order.clone()), "id-1", 100);
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.len(), 1);
        let (_, entry) = &snapshot[0];
        assert_eq!(entry.request().orders().len(), 1);
        assert_eq!(entry.solved_at_block(), 100);
        assert_eq!(entry.api_key_identity(), "id-1");
    }
}
