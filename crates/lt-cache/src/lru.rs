//! Byte-bounded in-memory LRU for decoded previews.
//!
//! O(1) get/insert/remove: a hash map from key to slot index plus an intrusive
//! doubly linked list over a slab of slots (no per-access allocation).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

/// Anything whose memory cost can be measured in bytes.
pub trait Weighted {
    fn weight(&self) -> usize;
}

impl Weighted for Vec<u8> {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl Weighted for [u8] {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl<T: Weighted + ?Sized> Weighted for Arc<T> {
    fn weight(&self) -> usize {
        (**self).weight()
    }
}

impl<T: Weighted + ?Sized> Weighted for Box<T> {
    fn weight(&self) -> usize {
        (**self).weight()
    }
}

const NIL: usize = usize::MAX;

struct Node<K, V> {
    key: K,
    val: V,
    weight: usize,
    prev: usize,
    next: usize,
}

/// Least-recently-used map bounded by the total [`Weighted::weight`] of its values.
pub struct MemLru<K, V> {
    map: HashMap<K, usize>,
    slots: Vec<Option<Node<K, V>>>,
    free: Vec<usize>,
    /// Most recently used.
    head: usize,
    /// Least recently used.
    tail: usize,
    weight: usize,
    capacity: usize,
}

impl<K, V> fmt::Debug for MemLru<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemLru")
            .field("len", &self.map.len())
            .field("weight", &self.weight)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl<K: Hash + Eq + Clone, V: Weighted> MemLru<K, V> {
    /// An empty cache holding at most `capacity_bytes` of values.
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            weight: 0,
            capacity: capacity_bytes,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Sum of the weights of all cached values.
    pub fn weight(&self) -> usize {
        self.weight
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Change the byte cap, evicting least-recently-used entries as needed.
    pub fn set_capacity(&mut self, capacity_bytes: usize) -> Vec<(K, V)> {
        self.capacity = capacity_bytes;
        self.evict_over_cap()
    }

    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(key)
    }

    /// Look up and mark as most recently used.
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let idx = *self.map.get(key)?;
        self.detach(idx);
        self.push_front(idx);
        self.slots[idx].as_ref().map(|n| &n.val)
    }

    /// Look up without changing recency.
    pub fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let idx = *self.map.get(key)?;
        self.slots[idx].as_ref().map(|n| &n.val)
    }

    /// Insert (or replace) `key` as most recently used and return whatever was
    /// evicted to stay under the cap. A value heavier than the whole cap is not
    /// stored and comes straight back in the returned list (any previous value
    /// for `key` is dropped).
    pub fn insert(&mut self, key: K, val: V) -> Vec<(K, V)> {
        let weight = val.weight();
        self.remove(&key);
        if weight > self.capacity {
            return vec![(key, val)];
        }
        let node = Node {
            key: key.clone(),
            val,
            weight,
            prev: NIL,
            next: NIL,
        };
        let idx = if let Some(i) = self.free.pop() {
            self.slots[i] = Some(node);
            i
        } else {
            self.slots.push(Some(node));
            self.slots.len() - 1
        };
        self.map.insert(key, idx);
        self.push_front(idx);
        self.weight += weight;
        self.evict_over_cap()
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let idx = self.map.remove(key)?;
        Some(self.take_slot(idx).1)
    }

    /// Remove every entry whose key matches `pred`.
    pub fn remove_where(&mut self, mut pred: impl FnMut(&K) -> bool) -> usize {
        let doomed: Vec<K> = self.map.keys().filter(|k| pred(k)).cloned().collect();
        for k in &doomed {
            self.remove(k);
        }
        doomed.len()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.slots.clear();
        self.free.clear();
        self.head = NIL;
        self.tail = NIL;
        self.weight = 0;
    }

    /// Keys from most to least recently used.
    pub fn keys_mru(&self) -> Vec<K> {
        let mut out = Vec::with_capacity(self.len());
        let mut i = self.head;
        while i != NIL {
            let n = self.slots[i].as_ref().expect("linked slot is occupied");
            out.push(n.key.clone());
            i = n.next;
        }
        out
    }

    fn evict_over_cap(&mut self) -> Vec<(K, V)> {
        let mut out = Vec::new();
        while self.weight > self.capacity && self.tail != NIL {
            let idx = self.tail;
            let (k, v) = self.take_slot(idx);
            self.map.remove(&k);
            out.push((k, v));
        }
        out
    }

    /// Unlink slot `idx`, free it, and return its contents. Caller maintains `map`.
    fn take_slot(&mut self, idx: usize) -> (K, V) {
        self.detach(idx);
        let n = self.slots[idx].take().expect("slot in use");
        self.free.push(idx);
        self.weight -= n.weight;
        (n.key, n.val)
    }

    fn detach(&mut self, idx: usize) {
        let (prev, next) = {
            let n = self.slots[idx].as_ref().expect("slot in use");
            (n.prev, n.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev].as_mut().expect("slot in use").next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next].as_mut().expect("slot in use").prev = prev;
        }
    }

    fn push_front(&mut self, idx: usize) {
        let old = self.head;
        {
            let n = self.slots[idx].as_mut().expect("slot in use");
            n.prev = NIL;
            n.next = old;
        }
        if old == NIL {
            self.tail = idx;
        } else {
            self.slots[old].as_mut().expect("slot in use").prev = idx;
        }
        self.head = idx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(n: usize) -> Vec<u8> {
        vec![0; n]
    }

    #[test]
    fn evicts_least_recently_used_by_weight() {
        let mut c = MemLru::new(30);
        assert_eq!(c.insert("a", v(10)), [] as [(&str, std::vec::Vec<u8>); 0]);
        assert_eq!(c.insert("b", v(10)), [] as [(&str, std::vec::Vec<u8>); 0]);
        assert_eq!(c.insert("c", v(10)), [] as [(&str, std::vec::Vec<u8>); 0]);
        assert_eq!(c.weight(), 30);
        // Touch "a" so "b" becomes the LRU.
        assert_eq!(c.get("a").map(Vec::len), Some(10));
        let ev = c.insert("d", v(5));
        assert_eq!(ev.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec!["b"]);
        assert_eq!(c.keys_mru(), vec!["d", "a", "c"]);
        assert_eq!(c.weight(), 25);
        // A big insert evicts several.
        let ev = c.insert("e", v(20));
        assert_eq!(
            ev.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec!["c", "a"]
        );
        assert_eq!(c.keys_mru(), vec!["e", "d"]);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn peek_does_not_promote() {
        let mut c = MemLru::new(20);
        c.insert(1, v(10));
        c.insert(2, v(10));
        assert!(c.peek(&1).is_some());
        let ev = c.insert(3, v(10));
        assert_eq!(ev[0].0, 1);
        assert!(c.peek(&99).is_none());
        assert!(c.get(&99).is_none());
    }

    #[test]
    fn oversized_value_is_rejected_and_old_value_dropped() {
        let mut c = MemLru::new(10);
        c.insert("k", v(4));
        let ev = c.insert("k", v(11));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].1.len(), 11);
        assert!(!c.contains("k"));
        assert_eq!(c.weight(), 0);
        assert!(c.is_empty());
    }

    #[test]
    fn replace_updates_weight_and_recency() {
        let mut c = MemLru::new(100);
        c.insert("a", v(10));
        c.insert("b", v(10));
        c.insert("a", v(30));
        assert_eq!(c.weight(), 40);
        assert_eq!(c.keys_mru(), vec!["a", "b"]);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn remove_clear_and_slot_reuse() {
        let mut c = MemLru::new(100);
        for i in 0..5 {
            c.insert(i, v(10));
        }
        assert_eq!(c.remove(&2).map(|x| x.len()), Some(10));
        assert_eq!(c.remove(&2), None);
        assert_eq!(c.keys_mru(), vec![4, 3, 1, 0]);
        // Removing head and tail keeps links consistent.
        c.remove(&4);
        c.remove(&0);
        assert_eq!(c.keys_mru(), vec![3, 1]);
        c.insert(7, v(1));
        assert_eq!(c.slots.len(), 5, "freed slots are reused");
        assert_eq!(c.remove_where(|k| *k % 2 == 1), 3);
        assert!(c.is_empty());
        c.insert(8, v(3));
        c.clear();
        assert_eq!((c.len(), c.weight()), (0, 0));
        assert_eq!(c.keys_mru(), [] as [i32; 0]);
        c.insert(9, v(3));
        assert_eq!(c.keys_mru(), vec![9]);
    }

    #[test]
    fn shrinking_capacity_evicts() {
        let mut c = MemLru::new(100);
        for i in 0..10 {
            c.insert(i, v(10));
        }
        let ev = c.set_capacity(35);
        assert_eq!(
            ev.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
        assert_eq!(c.capacity(), 35);
        assert_eq!(c.weight(), 30);
    }

    #[test]
    fn weighted_impls_and_debug() {
        let a: Arc<Vec<u8>> = Arc::new(v(7));
        assert_eq!(a.weight(), 7);
        let b: Box<[u8]> = vec![1, 2, 3].into_boxed_slice();
        assert_eq!(b.weight(), 3);
        let arc_slice: Arc<[u8]> = Arc::from(&[1u8, 2][..]);
        assert_eq!(arc_slice.weight(), 2);
        let mut c: MemLru<String, Arc<Vec<u8>>> = MemLru::new(8);
        c.insert("x".into(), a);
        assert_eq!(c.get("x").map(|x| x.len()), Some(7));
        assert!(format!("{c:?}").contains("weight: 7"));
    }

    #[test]
    fn randomized_against_model() {
        // Deterministic LCG; model = Vec in MRU order.
        let mut seed = 0x1234_5678_u64;
        let mut rnd = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        let cap = 50;
        let mut c = MemLru::new(cap);
        let mut model: Vec<(u64, usize)> = Vec::new();
        for _ in 0..5_000 {
            let k = rnd(20);
            match rnd(3) {
                0 => {
                    let w = rnd(20) as usize;
                    c.insert(k, v(w));
                    model.retain(|(mk, _)| *mk != k);
                    if w <= cap {
                        model.insert(0, (k, w));
                        while model.iter().map(|(_, w)| w).sum::<usize>() > cap {
                            model.pop();
                        }
                    }
                }
                1 => {
                    let got = c.get(&k).map(Vec::len);
                    let pos = model.iter().position(|(mk, _)| *mk == k);
                    assert_eq!(got, pos.map(|p| model[p].1));
                    if let Some(p) = pos {
                        let e = model.remove(p);
                        model.insert(0, e);
                    }
                }
                _ => {
                    let got = c.remove(&k).map(|x| x.len());
                    let pos = model.iter().position(|(mk, _)| *mk == k);
                    assert_eq!(got, pos.map(|p| model.remove(p).1));
                }
            }
            assert_eq!(
                c.keys_mru(),
                model.iter().map(|(k, _)| *k).collect::<Vec<_>>()
            );
            assert_eq!(c.weight(), model.iter().map(|(_, w)| w).sum::<usize>());
        }
    }
}
