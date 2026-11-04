//! Values kept within a byte budget and an entry count, least recently used out first (server/lru.py).
use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

pub struct LruCache<K, V> {
    pub budget_bytes: usize,
    pub capacity: Option<usize>,
    pub bytes: usize,
    /// The entries dropped to make room.
    pub evictions: u64,
    entries: HashMap<K, (V, usize, u64)>,
    order: BTreeMap<u64, K>,
    tick: u64,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    pub fn new(budget_bytes: usize, capacity: Option<usize>) -> Self {
        LruCache { budget_bytes, capacity, bytes: 0, evictions: 0, entries: HashMap::new(), order: BTreeMap::new(), tick: 0 }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value kept for `key`, now the most recently used.
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.tick += 1;
        let tick = self.tick;
        let (_, _, used) = self.entries.get_mut(key)?;
        let owned = self.order.remove(used).expect("ordered entry");
        *used = tick;
        self.order.insert(tick, owned);
        self.entries.get(key).map(|(v, _, _)| v)
    }

    /// Keeps `value` of `size` bytes for `key`, dropping the least recently used until both limits hold; false, keeping
    /// nothing, when it alone outgrows the budget.
    pub fn put(&mut self, key: K, value: V, size: usize) -> bool {
        if size > self.budget_bytes {
            return false;
        }
        self.pop(&key);
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.entries.insert(key, (value, size, self.tick));
        self.bytes += size;
        while self.bytes > self.budget_bytes || self.capacity.is_some_and(|c| self.entries.len() > c) {
            let (_, oldest) = self.order.pop_first().expect("an entry to evict");
            let (_, evicted, _) = self.entries.remove(&oldest).expect("ordered entry");
            self.bytes -= evicted;
            self.evictions += 1;
        }
        true
    }

    pub fn pop<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let (value, size, used) = self.entries.remove(key)?;
        self.order.remove(&used);
        self.bytes -= size;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::LruCache;

    #[test]
    fn evicts_least_recently_used_within_budget_and_capacity() {
        let mut cache = LruCache::new(10, Some(3));
        assert!(cache.put("a", 1, 4));
        assert!(cache.put("b", 2, 4));
        assert_eq!(cache.get("a"), Some(&1));
        assert!(cache.put("c", 3, 4)); // 12 bytes: b goes
        assert_eq!(cache.get("b"), None);
        assert_eq!((cache.len(), cache.bytes, cache.evictions), (2, 8, 1));
        assert!(!cache.put("huge", 9, 11));
        assert!(cache.put("d", 4, 1));
        assert!(cache.put("e", 5, 1)); // capacity 3: a, the oldest, goes
        assert_eq!(cache.get("a"), None);
        assert_eq!(cache.len(), 3);
    }
}
