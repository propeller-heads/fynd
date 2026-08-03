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
}
