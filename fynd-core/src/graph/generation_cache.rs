//! Memo for values derived from a graph, keyed by the graph's generation.

use std::{collections::HashMap, hash::Hash};

/// Caches values derived from a graph, discarding them whenever the generation changes.
///
/// Entries are built lazily per key and dropped in bulk: a generation mismatch on access clears
/// everything before the lookup, so a value derived from an older topology can never be returned.
pub(crate) struct GenerationCache<K, V> {
    generation: u64,
    entries: HashMap<K, V>,
}

impl<K: Eq + Hash, V: Clone> GenerationCache<K, V> {
    pub(crate) fn new() -> Self {
        Self { generation: 0, entries: HashMap::new() }
    }

    /// Returns the value for `key`, building it if this generation has not produced it yet.
    pub(crate) fn get_or_insert_with(
        &mut self,
        generation: u64,
        key: K,
        build: impl FnOnce() -> V,
    ) -> V {
        if self.generation != generation {
            self.entries.clear();
            self.generation = generation;
        }
        self.entries
            .entry(key)
            .or_insert_with(build)
            .clone()
    }
}

impl<K: Eq + Hash, V: Clone> Default for GenerationCache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Memoises the value for the most recent key only: a different key, or a new generation,
/// replaces it.
///
/// This is the bounded counterpart to [`GenerationCache`], for values large enough that keeping
/// one per key would grow with request diversity rather than with the graph — a generation can
/// span many blocks, so nothing would release them. Holding a single entry bounds the cache by
/// construction, with no capacity to tune and no eviction policy to reason about.
///
/// It suits callers that query one key per order: there is no second key to displace the first
/// mid-order, so what a single slot captures is repetition from one order to the next.
pub(crate) struct LastQueryCache<K, V> {
    entry: Option<(u64, K, V)>,
}

impl<K: Eq, V: Clone> LastQueryCache<K, V> {
    pub(crate) fn new() -> Self {
        Self { entry: None }
    }

    /// Returns the value for `key`, rebuilding and replacing the entry unless it already holds
    /// this key at this generation.
    pub(crate) fn get_or_replace(
        &mut self,
        generation: u64,
        key: K,
        build: impl FnOnce() -> V,
    ) -> V {
        if let Some((cached_generation, cached_key, value)) = &self.entry {
            if *cached_generation == generation && *cached_key == key {
                return value.clone();
            }
        }
        let value = build();
        self.entry = Some((generation, key, value.clone()));
        value
    }
}

impl<K: Eq, V: Clone> Default for LastQueryCache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn test_second_access_in_same_generation_reuses_value() {
        let mut cache: GenerationCache<u8, u32> = GenerationCache::new();
        let builds = Cell::new(0);

        let first = cache.get_or_insert_with(7, 1, || {
            builds.set(builds.get() + 1);
            42
        });
        let second = cache.get_or_insert_with(7, 1, || {
            builds.set(builds.get() + 1);
            99
        });

        assert_eq!(first, 42);
        assert_eq!(second, 42);
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn test_distinct_keys_are_cached_separately() {
        let mut cache: GenerationCache<u8, u32> = GenerationCache::new();

        assert_eq!(cache.get_or_insert_with(7, 1, || 10), 10);
        assert_eq!(cache.get_or_insert_with(7, 2, || 20), 20);
        assert_eq!(cache.get_or_insert_with(7, 1, || 0), 10);
    }

    #[test]
    fn test_new_generation_rebuilds_every_key() {
        let mut cache: GenerationCache<u8, u32> = GenerationCache::new();
        cache.get_or_insert_with(7, 1, || 10);
        cache.get_or_insert_with(7, 2, || 20);

        assert_eq!(cache.get_or_insert_with(8, 1, || 11), 11);
        assert_eq!(cache.get_or_insert_with(8, 2, || 22), 22);
    }

    #[test]
    fn test_returning_to_an_earlier_generation_still_rebuilds() {
        let mut cache: GenerationCache<u8, u32> = GenerationCache::new();
        cache.get_or_insert_with(7, 1, || 10);
        cache.get_or_insert_with(8, 1, || 11);

        assert_eq!(cache.get_or_insert_with(7, 1, || 12), 12);
    }

    #[test]
    fn test_last_query_reuses_a_repeated_key() {
        let mut cache: LastQueryCache<u8, u32> = LastQueryCache::new();
        let builds = Cell::new(0);

        let first = cache.get_or_replace(7, 1, || {
            builds.set(builds.get() + 1);
            42
        });
        let second = cache.get_or_replace(7, 1, || {
            builds.set(builds.get() + 1);
            99
        });

        assert_eq!(first, 42);
        assert_eq!(second, 42);
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn test_last_query_rebuilds_for_a_different_key() {
        let mut cache: LastQueryCache<u8, u32> = LastQueryCache::new();

        assert_eq!(cache.get_or_replace(7, 1, || 10), 10);
        assert_eq!(cache.get_or_replace(7, 2, || 20), 20);
    }

    #[test]
    fn test_last_query_keeps_only_the_most_recent_key() {
        // The bound: revisiting a displaced key rebuilds rather than reading a retained entry.
        let mut cache: LastQueryCache<u8, u32> = LastQueryCache::new();
        cache.get_or_replace(7, 1, || 10);
        cache.get_or_replace(7, 2, || 20);

        assert_eq!(cache.get_or_replace(7, 1, || 11), 11);
    }

    #[test]
    fn test_last_query_rebuilds_on_a_new_generation() {
        let mut cache: LastQueryCache<u8, u32> = LastQueryCache::new();
        cache.get_or_replace(7, 1, || 10);

        assert_eq!(cache.get_or_replace(8, 1, || 11), 11);
    }
}
