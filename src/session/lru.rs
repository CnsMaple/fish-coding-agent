//! Tiny bounded cache keyed by `usize` with FIFO-ish eviction.
//!
//! Used by the session renderer to keep a bounded number of fully
//! rendered `Vec<Line>` per message in memory. The implementation is
//! `HashMap` + `VecDeque` for insertion order; on overflow the oldest
//! insertion is removed. This is not a strict LRU but is good enough
//! for keeping recent viewport windows hot without unbounded growth.

use std::collections::{HashMap, VecDeque};

#[derive(Debug)]
pub struct BoundedCache<V> {
    map: HashMap<usize, V>,
    order: VecDeque<usize>,
    capacity: usize,
}

impl<V> Default for BoundedCache<V> {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}

impl<V> BoundedCache<V> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, key: &usize) -> Option<&V> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &usize) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    pub fn contains(&self, key: &usize) -> bool {
        self.map.contains_key(key)
    }

    /// Iterate over all cached keys. Used by callers that need to
    /// selectively evict entries (e.g. invalidating indices that
    /// shifted after a `truncate` / `remove` on the source list).
    pub fn iter_keys(&self) -> impl Iterator<Item = &usize> {
        self.map.keys()
    }

    /// Insert `value` for `key`. If the cache is at capacity, evict
    /// the oldest entry. Returns the evicted key/value, if any.
    pub fn put(&mut self, key: usize, value: V) -> Option<(usize, V)> {
        if self.capacity == 0 {
            return Some((key, value));
        }
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(key) {
            // Update in place; do not move in eviction order.
            e.insert(value);
            return None;
        }
        let mut evicted = None;
        if self.map.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(v) = self.map.remove(&oldest) {
                    evicted = Some((oldest, v));
                }
            }
        }
        self.map.insert(key, value);
        self.order.push_back(key);
        evicted
    }

    pub fn remove(&mut self, key: &usize) -> Option<V> {
        if let Some(v) = self.map.remove(key) {
            if let Some(pos) = self.order.iter().position(|&k| k == *key) {
                self.order.remove(pos);
            }
            Some(v)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_on_overflow() {
        let mut c: BoundedCache<u32> = BoundedCache::with_capacity(2);
        c.put(1, 10);
        c.put(2, 20);
        let evicted = c.put(3, 30);
        assert_eq!(evicted, Some((1, 10)));
        assert_eq!(c.get(&1), None);
        assert_eq!(c.get(&2), Some(&20));
        assert_eq!(c.get(&3), Some(&30));
    }

    #[test]
    fn update_does_not_evict() {
        let mut c: BoundedCache<u32> = BoundedCache::with_capacity(2);
        c.put(1, 10);
        c.put(2, 20);
        let evicted = c.put(1, 11);
        assert_eq!(evicted, None);
        assert_eq!(c.get(&1), Some(&11));
        assert_eq!(c.get(&2), Some(&20));
    }

    #[test]
    fn remove_keeps_order() {
        let mut c: BoundedCache<u32> = BoundedCache::with_capacity(2);
        c.put(1, 10);
        c.put(2, 20);
        c.remove(&1);
        let evicted = c.put(3, 30);
        assert_eq!(evicted, None);
        assert_eq!(c.get(&2), Some(&20));
        assert_eq!(c.get(&3), Some(&30));
    }
}
