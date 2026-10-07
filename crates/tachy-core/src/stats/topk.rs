//! Most frequent values of a column (spec §7.2).
//!
//! Two counters, chosen by where the stats come from:
//! - **Sample** ([`TopK::Exact`]): an exact `HashMap` of value → count. The
//!   sample is at most 20k rows, so this stays small.
//! - **Full profile** ([`TopK::SpaceSaving`]): the Space-Saving algorithm with
//!   capacity [`SPACE_SAVING_CAPACITY`] (1,024 counters). When the column has at
//!   most 1,024 distinct values every count is exact. Otherwise each reported
//!   count can be **too high** by at most its `max_overcount`, which is never
//!   more than the smallest counter; the inspector marks such counts with `~`.
//!
//! In both modes, values longer than [`MAX_KEY_LEN`] bytes are cut to 256
//! bytes plus a `…` marker before counting, so a long-text column cannot use
//! much memory. Two long values that share their first 256 bytes therefore
//! count as one.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
};

use super::Accumulate;
use crate::types::Value;

/// Longest value counted as is; longer values are truncated.
pub const MAX_KEY_LEN: usize = 256;
/// Appended to truncated values (`…` in UTF-8).
pub const TRUNCATION_MARKER: &[u8] = "\u{2026}".as_bytes();
/// Counters kept by the Space-Saving summary.
pub const SPACE_SAVING_CAPACITY: usize = 1024;

/// The key a value is counted under: the value itself, or its first 256 bytes
/// plus [`TRUNCATION_MARKER`].
pub fn count_key(v: &[u8]) -> Box<[u8]> {
    if v.len() <= MAX_KEY_LEN {
        v.into()
    } else {
        let mut k = Vec::with_capacity(MAX_KEY_LEN + TRUNCATION_MARKER.len());
        k.extend_from_slice(&v[..MAX_KEY_LEN]);
        k.extend_from_slice(TRUNCATION_MARKER);
        k.into_boxed_slice()
    }
}

/// One entry of a top-k list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopEntry {
    /// The value (possibly truncated, see [`count_key`]).
    pub value: Box<[u8]>,
    /// Its count. With Space-Saving this is an upper bound.
    pub count: u64,
    /// How much `count` may exceed the true count (0 = exact).
    pub max_overcount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Counter {
    key: Box<[u8]>,
    count: u64,
    err: u64,
}

/// A Space-Saving summary (Metwally et al.), mergeable as in Agarwal et al.,
/// "Mergeable Summaries".
///
/// Guarantees for every value `x` with true count `f(x)`:
/// - if `x` is tracked: `count − max_overcount ≤ f(x) ≤ count`;
/// - if not: `f(x) ≤` [`SpaceSaving::min_count`].
#[derive(Clone, Debug)]
pub struct SpaceSaving {
    capacity: usize,
    slots: Vec<Counter>,
    index: HashMap<Box<[u8]>, usize>,
    /// Lazy min-heap of `(count, slot)`. Entries whose count no longer matches
    /// the slot are stale and skipped.
    heap: BinaryHeap<Reverse<(u64, usize)>>,
}

impl PartialEq for SpaceSaving {
    /// Two summaries are equal when they track the same counters.
    fn eq(&self, other: &SpaceSaving) -> bool {
        self.capacity == other.capacity && self.sorted_counters() == other.sorted_counters()
    }
}

impl Default for SpaceSaving {
    fn default() -> SpaceSaving {
        SpaceSaving::with_capacity(SPACE_SAVING_CAPACITY)
    }
}

impl SpaceSaving {
    /// An empty summary with `capacity` counters (at least 1).
    pub fn with_capacity(capacity: usize) -> SpaceSaving {
        SpaceSaving {
            capacity: capacity.max(1),
            slots: Vec::new(),
            index: HashMap::new(),
            heap: BinaryHeap::new(),
        }
    }

    /// The number of counters.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether every counter is in use.
    pub fn is_full(&self) -> bool {
        self.slots.len() >= self.capacity
    }

    /// The bound on the count of any untracked value: the smallest counter
    /// when full, else 0.
    pub fn min_count(&self) -> u64 {
        if self.is_full() {
            self.slots.iter().map(|c| c.count).min().unwrap_or(0)
        } else {
            0
        }
    }

    /// Counts one occurrence of `v`.
    pub fn insert(&mut self, v: &[u8]) {
        self.add(count_key(v), 1);
    }

    fn push_heap(&mut self, count: u64, slot: usize) {
        self.heap.push(Reverse((count, slot)));
        if self.heap.len() > 4 * self.capacity + 16 {
            self.heap = self
                .slots
                .iter()
                .enumerate()
                .map(|(i, c)| Reverse((c.count, i)))
                .collect();
        }
    }

    /// Adds `weight` occurrences of an already truncated key.
    fn add(&mut self, key: Box<[u8]>, weight: u64) {
        if let Some(&slot) = self.index.get(&key) {
            let c = &mut self.slots[slot];
            c.count += weight;
            let count = c.count;
            self.push_heap(count, slot);
            return;
        }
        if !self.is_full() {
            let slot = self.slots.len();
            self.index.insert(key.clone(), slot);
            self.slots.push(Counter {
                key,
                count: weight,
                err: 0,
            });
            self.push_heap(weight, slot);
            return;
        }
        // Evict the smallest counter. Counts in a slot only ever grow, so an
        // entry whose count matches the slot is current.
        let (min, slot) = loop {
            let Reverse((count, slot)) = self.heap.pop().expect("heap tracks every slot");
            if self.slots[slot].count == count {
                break (count, slot);
            }
        };
        let old = std::mem::replace(
            &mut self.slots[slot],
            Counter {
                key: key.clone(),
                count: min + weight,
                err: min,
            },
        );
        self.index.remove(&old.key);
        self.index.insert(key, slot);
        self.push_heap(min + weight, slot);
    }

    fn sorted_counters(&self) -> Vec<&Counter> {
        let mut v: Vec<&Counter> = self.slots.iter().collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        v
    }

    /// The `n` largest counters, by count descending, then value ascending.
    pub fn top(&self, n: usize) -> Vec<TopEntry> {
        self.sorted_counters()
            .into_iter()
            .take(n)
            .map(|c| TopEntry {
                value: c.key.clone(),
                count: c.count,
                max_overcount: c.err,
            })
            .collect()
    }

    /// The estimate and over-count bound for `v`, if it is tracked.
    pub fn get(&self, v: &[u8]) -> Option<(u64, u64)> {
        let slot = *self.index.get(&count_key(v)[..])?;
        let c = &self.slots[slot];
        Some((c.count, c.err))
    }

    fn rebuild(capacity: usize, mut counters: Vec<Counter>) -> SpaceSaving {
        counters.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        counters.truncate(capacity);
        let mut out = SpaceSaving::with_capacity(capacity);
        for (i, c) in counters.into_iter().enumerate() {
            out.index.insert(c.key.clone(), i);
            out.heap.push(Reverse((c.count, i)));
            out.slots.push(c);
        }
        out
    }

    fn merge_summary(&mut self, other: SpaceSaving) {
        let m1 = self.min_count();
        let m2 = other.min_count();
        let capacity = self.capacity.max(other.capacity);
        let mut merged: HashMap<Box<[u8]>, (u64, u64)> = HashMap::new();
        for c in std::mem::take(&mut self.slots) {
            merged.insert(c.key, (c.count, c.err));
        }
        let mut seen_in_other = std::collections::HashSet::new();
        for c in other.slots {
            seen_in_other.insert(c.key.clone());
            let e = merged.entry(c.key).or_insert((m1, m1));
            e.0 += c.count;
            e.1 += c.err;
        }
        let counters = merged
            .into_iter()
            .map(|(key, (count, err))| {
                let (count, err) = if seen_in_other.contains(&key) {
                    (count, err)
                } else {
                    (count + m2, err + m2)
                };
                Counter { key, count, err }
            })
            .collect();
        *self = SpaceSaving::rebuild(capacity, counters);
    }
}

/// Top-value counter for a column: exact for a sample, Space-Saving for a full
/// profile (see the module docs).
#[derive(Clone, Debug, PartialEq)]
pub enum TopK {
    /// Exact counts (sample stats).
    Exact(HashMap<Box<[u8]>, u64>),
    /// Approximate counts (full profile).
    SpaceSaving(SpaceSaving),
}

impl Default for TopK {
    fn default() -> TopK {
        TopK::exact()
    }
}

impl TopK {
    /// An exact counter.
    pub fn exact() -> TopK {
        TopK::Exact(HashMap::new())
    }

    /// A Space-Saving counter with the default capacity (1,024).
    pub fn space_saving() -> TopK {
        TopK::SpaceSaving(SpaceSaving::default())
    }

    /// Counts one occurrence of `v`.
    pub fn insert(&mut self, v: &[u8]) {
        match self {
            TopK::Exact(map) => {
                if let Some(c) = map.get_mut(v) {
                    *c += 1;
                } else {
                    *map.entry(count_key(v)).or_insert(0) += 1;
                }
            }
            TopK::SpaceSaving(ss) => ss.insert(v),
        }
    }

    /// Whether some reported counts may be over-estimates.
    pub fn is_approximate(&self) -> bool {
        match self {
            TopK::Exact(_) => false,
            TopK::SpaceSaving(ss) => ss.is_full(),
        }
    }

    /// The `n` most frequent values, by count descending, then value
    /// ascending.
    pub fn top(&self, n: usize) -> Vec<TopEntry> {
        match self {
            TopK::Exact(map) => {
                let mut v: Vec<(&Box<[u8]>, &u64)> = map.iter().collect();
                v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                v.into_iter()
                    .take(n)
                    .map(|(k, &c)| TopEntry {
                        value: k.clone(),
                        count: c,
                        max_overcount: 0,
                    })
                    .collect()
            }
            TopK::SpaceSaving(ss) => ss.top(n),
        }
    }

    fn into_space_saving(self) -> SpaceSaving {
        match self {
            TopK::SpaceSaving(ss) => ss,
            TopK::Exact(map) => {
                let mut counters: Vec<(Box<[u8]>, u64)> = map.into_iter().collect();
                counters.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let mut ss = SpaceSaving::default();
                for (k, c) in counters {
                    ss.add(k, c);
                }
                ss
            }
        }
    }
}

impl Accumulate for TopK {
    /// Counts non-null values only, on their raw bytes.
    fn push(&mut self, v: &[u8], parsed: &Value) {
        if !matches!(parsed, Value::Null) {
            self.insert(v);
        }
    }

    /// Exact + exact stays exact. Otherwise both sides become Space-Saving
    /// summaries and are merged.
    fn merge(&mut self, other: TopK) {
        match (&mut *self, other) {
            (TopK::Exact(a), TopK::Exact(b)) => {
                for (k, c) in b {
                    *a.entry(k).or_insert(0) += c;
                }
            }
            (_, other) => {
                let mut ss = std::mem::take(self).into_space_saving();
                ss.merge_summary(other.into_space_saving());
                *self = TopK::SpaceSaving(ss);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(t: &TopK, n: usize) -> Vec<(String, u64)> {
        t.top(n)
            .into_iter()
            .map(|e| (String::from_utf8_lossy(&e.value).into_owned(), e.count))
            .collect()
    }

    #[test]
    fn exact_counts_and_order() {
        let mut t = TopK::exact();
        for v in ["b", "a", "c", "a", "b", "a", "", "d"] {
            t.insert(v.as_bytes());
        }
        assert_eq!(
            entries(&t, 3),
            vec![("a".into(), 3), ("b".into(), 2), ("".into(), 1)]
        );
        assert!(!t.is_approximate());
    }

    #[test]
    fn long_values_are_truncated() {
        let long_a = vec![b'x'; 1000];
        let mut long_b = vec![b'x'; 300];
        long_b.push(b'y');
        let mut t = TopK::exact();
        t.insert(&long_a);
        t.insert(&long_b);
        let top = t.top(5);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].count, 2);
        assert_eq!(top[0].value.len(), MAX_KEY_LEN + TRUNCATION_MARKER.len());
        assert!(top[0].value.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn space_saving_is_exact_up_to_capacity() {
        let mut ss = SpaceSaving::with_capacity(16);
        for i in 0..16u32 {
            for _ in 0..=i {
                ss.insert(&i.to_le_bytes());
            }
        }
        let top = ss.top(3);
        assert_eq!(top[0].count, 16);
        assert_eq!(top[0].max_overcount, 0);
        assert_eq!(top[2].count, 14);
    }

    #[test]
    fn space_saving_bounds_hold_after_eviction() {
        let mut ss = SpaceSaving::with_capacity(8);
        let mut truth: HashMap<u32, u64> = HashMap::new();
        let mut x = 12345u32;
        for _ in 0..5_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            // Skewed: small values are much more frequent.
            let v = (x >> 16) % 64;
            let v = v * v / 64;
            ss.insert(&v.to_le_bytes());
            *truth.entry(v).or_insert(0) += 1;
        }
        let min = ss.min_count();
        for (v, &f) in &truth {
            match ss.get(&v.to_le_bytes()) {
                Some((c, e)) => assert!(c - e <= f && f <= c, "{v}: {c}-{e} vs {f}"),
                None => assert!(f <= min, "{v}: {f} > {min}"),
            }
        }
        // The heaviest value is found.
        let heaviest = truth.iter().max_by_key(|(_, c)| **c).unwrap().0;
        assert_eq!(&ss.top(1)[0].value[..], &heaviest.to_le_bytes()[..]);
    }

    #[test]
    fn exact_merge_equals_whole() {
        let mut a = TopK::exact();
        let mut b = TopK::exact();
        let mut whole = TopK::exact();
        for i in 0..1000u32 {
            let v = (i % 37).to_le_bytes();
            whole.insert(&v);
            if i < 400 { a.insert(&v) } else { b.insert(&v) }
        }
        a.merge(b);
        assert_eq!(a, whole);
    }

    #[test]
    fn mixed_merge_becomes_space_saving() {
        let mut a = TopK::exact();
        a.insert(b"x");
        let mut b = TopK::space_saving();
        b.insert(b"x");
        b.insert(b"y");
        a.merge(b);
        assert!(matches!(a, TopK::SpaceSaving(_)));
        assert_eq!(entries(&a, 5), vec![("x".into(), 2), ("y".into(), 1)]);
    }
}
