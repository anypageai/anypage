//! A sharded, size-bounded cache shared by every thread that draws text.
//!
//! Small entries live in shards of two generations: lookups hit the current
//! one, or promote an entry from the previous one; when the current
//! generation exceeds its budget it becomes the previous one and the oldest
//! entries are dropped. That approximates LRU with O(1) work and no
//! per-entry bookkeeping.
//!
//! Large entries (pieces of long paragraphs, glyph images of big colour
//! glyphs) would fill a shard generation on their own and evict everything
//! next to them, themselves included, on the next insert. They go to one
//! separate LRU with its own byte budget instead, where the entry just
//! inserted is never the one evicted.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rustc_hash::FxBuildHasher;

const SHARDS: usize = 16;

type Map<K, V> = HashMap<K, (V, usize), FxBuildHasher>;

struct Generations<K, V> {
    current: Map<K, V>,
    previous: Map<K, V>,
    cost: usize,
}

/// LRU for large entries: value, cost, last use.
struct Large<K, V> {
    map: HashMap<K, (V, usize, u64), FxBuildHasher>,
    cost: usize,
    tick: u64,
}

pub(crate) struct Cache<K, V> {
    shards: Vec<Mutex<Generations<K, V>>>,
    /// Budget per shard and generation, in the caller's cost units.
    budget: usize,
    large: Mutex<Large<K, V>>,
    /// Entries costing more than this go to `large`.
    large_from: usize,
    large_budget: usize,
    hasher: FxBuildHasher,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves a consistent map (inserts are
    // single calls), so a poisoned lock is safe to keep using.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<K: Hash + Eq + Clone, V: Clone> Cache<K, V> {
    /// `total` bounds the summed cost of the small entries (both
    /// generations of all shards); `large` bounds the large entries, except
    /// that the most recently inserted one is always kept.
    pub fn new(total: usize, large: usize) -> Self {
        let shards = (0..SHARDS)
            .map(|_| Mutex::new(Generations { current: Map::default(), previous: Map::default(), cost: 0 }))
            .collect();
        let budget = (total / (2 * SHARDS)).max(1);
        Self {
            shards,
            budget,
            large: Mutex::new(Large { map: HashMap::default(), cost: 0, tick: 0 }),
            large_from: (budget / 8).max(1),
            large_budget: large,
            hasher: FxBuildHasher,
        }
    }

    fn shard<Q: Hash + ?Sized>(&self, k: &Q) -> &Mutex<Generations<K, V>> {
        let h = self.hasher.hash_one(k);
        &self.shards[((h >> 32) ^ h) as usize % SHARDS]
    }

    pub fn get<Q>(&self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        {
            let mut g = lock(self.shard(k));
            if let Some((v, _)) = g.current.get(k) {
                return Some(v.clone());
            }
            if let Some((key, (v, cost))) = g.previous.remove_entry(k) {
                let out = v.clone();
                Self::put(&mut g, key, v, cost, self.budget);
                return Some(out);
            }
        }
        let mut l = lock(&self.large);
        l.tick += 1;
        let tick = l.tick;
        let e = l.map.get_mut(k)?;
        e.2 = tick;
        Some(e.0.clone())
    }

    pub fn insert(&self, k: K, v: V, cost: usize) {
        if cost > self.large_from {
            self.insert_large(k, v, cost);
            return;
        }
        let mut g = lock(self.shard(&k));
        Self::put(&mut g, k, v, cost, self.budget);
    }

    fn put(g: &mut Generations<K, V>, k: K, v: V, cost: usize, budget: usize) {
        if let Some((_, old)) = g.current.insert(k, (v, cost)) {
            g.cost = g.cost.saturating_sub(old);
        }
        g.cost += cost;
        if g.cost > budget {
            g.previous = std::mem::take(&mut g.current);
            g.cost = 0;
        }
    }

    fn insert_large(&self, k: K, v: V, cost: usize) {
        let mut l = lock(&self.large);
        l.tick += 1;
        let tick = l.tick;
        if let Some((_, old, _)) = l.map.insert(k.clone(), (v, cost, tick)) {
            l.cost = l.cost.saturating_sub(old);
        }
        l.cost += cost;
        // Evict the least recently used, never the entry just inserted.
        while l.cost > self.large_budget && l.map.len() > 1 {
            let Some(oldest) = l.map.iter().filter(|(key, _)| **key != k).min_by_key(|(_, e)| e.2).map(|(key, _)| key.clone()) else { break };
            if let Some((_, c, _)) = l.map.remove(&oldest) {
                l.cost = l.cost.saturating_sub(c);
            }
        }
    }

    /// (entries, summed cost) over both generations and the large entries.
    pub fn stats(&self) -> (usize, usize) {
        let (n, c) = self.shards.iter().fold((0, 0), |(n, c), s| {
            let g = lock(s);
            let cost: usize = g.current.values().chain(g.previous.values()).map(|(_, c)| *c).sum();
            (n + g.current.len() + g.previous.len(), c + cost)
        });
        let l = lock(&self.large);
        (n + l.map.len(), c + l.cost)
    }

    pub fn clear(&self) {
        for s in &self.shards {
            let mut g = lock(s);
            g.current.clear();
            g.previous.clear();
            g.cost = 0;
        }
        let mut l = lock(&self.large);
        l.map.clear();
        l.cost = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_promotes() {
        let c: Cache<String, u32> = Cache::new(SHARDS * 2 * 10, 0);
        for i in 0..10_000u32 {
            c.insert(format!("k{i}"), i, 1);
        }
        let (n, cost) = c.stats();
        assert!(n <= SHARDS * 2 * 11, "{n} entries kept");
        assert!(cost <= SHARDS * 2 * 11);
        // the newest entries survive
        assert_eq!(c.get("k9999"), Some(9999));
        c.clear();
        assert_eq!(c.get("k9999"), None);
    }

    #[test]
    fn replacing_an_entry_keeps_cost_exact() {
        let c: Cache<&'static str, u32> = Cache::new(1 << 20, 0);
        c.insert("a", 1, 100);
        c.insert("a", 2, 50);
        assert_eq!(c.stats(), (1, 50));
        assert_eq!(c.get("a"), Some(2));
    }

    #[test]
    fn large_entries_are_kept_by_recency_not_evicted_on_insert() {
        // shard budget 1000 per generation: entries over 125 are large
        let c: Cache<u32, u32> = Cache::new(SHARDS * 2 * 1000, 10_000);
        // one entry larger than a whole shard generation stays
        c.insert(1, 1, 5_000);
        c.insert(2, 2, 5_000);
        assert_eq!(c.get(&1), Some(1));
        assert_eq!(c.get(&2), Some(2));
        // a third evicts the least recently used one (1 was read before 2)
        c.insert(3, 3, 5_000);
        assert_eq!(c.get(&1), None);
        assert_eq!(c.get(&2), Some(2));
        assert_eq!(c.get(&3), Some(3));
        // one entry over the whole large budget is still kept (the newest)
        c.insert(4, 4, 50_000);
        assert_eq!(c.get(&4), Some(4));
        assert_eq!(c.stats().0, 1);
        // small entries are untouched by large ones
        c.insert(10, 10, 1);
        c.insert(5, 5, 50_000);
        assert_eq!(c.get(&10), Some(10));
    }
}
