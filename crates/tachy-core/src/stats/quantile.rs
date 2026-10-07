//! Quantiles (p50, p95) for numeric columns (spec §7.2).
//!
//! - **Sample** ([`Quantiles::Exact`]): every parsed value is kept (at most
//!   20k) and the quantile is computed exactly with `select_nth_unstable`,
//!   interpolating linearly between neighbours (the "R-7" definition used by
//!   most spreadsheets and NumPy).
//! - **Full profile** ([`Quantiles::Sketch`]): a hand-written KLL sketch
//!   (Karnin, Lang, Liberty 2016) with `k = 200`, about 1.3 % rank error. It is
//!   mergeable, and the inspector marks its values as approximate
//!   (`p50 ~1,204.5`).
//!
//! Values are `f64`; `i64` columns are converted, which is exact up to 2^53.

use super::Accumulate;
use crate::types::Value;

/// The KLL `k` parameter.
pub const KLL_K: usize = 200;

/// Exact quantiles over all pushed values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExactQuantiles {
    values: Vec<f64>,
}

impl ExactQuantiles {
    /// An empty set of values.
    pub fn new() -> ExactQuantiles {
        ExactQuantiles::default()
    }

    /// Adds a value.
    pub fn insert(&mut self, x: f64) {
        self.values.push(x);
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no value was added.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The `q`-quantile (`0.0..=1.0`), linearly interpolated (R-7). `None`
    /// when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        let mut v = self.values.clone();
        let h = (v.len() - 1) as f64 * q.clamp(0.0, 1.0);
        let lo = h.floor() as usize;
        let frac = h - lo as f64;
        let (_, &mut lo_v, upper) = v.select_nth_unstable_by(lo, f64::total_cmp);
        if frac == 0.0 || upper.is_empty() {
            return Some(lo_v);
        }
        let hi_v = upper.iter().copied().min_by(f64::total_cmp).unwrap_or(lo_v);
        Some(lo_v + frac * (hi_v - lo_v))
    }
}

/// A KLL quantile sketch with a deterministic coin, so results are
/// reproducible.
#[derive(Clone, Debug, PartialEq)]
pub struct Kll {
    k: usize,
    levels: Vec<Vec<f64>>,
    size: usize,
    max_size: usize,
    n: u64,
    rng: u64,
}

impl Default for Kll {
    fn default() -> Kll {
        Kll::new(KLL_K)
    }
}

impl Kll {
    /// An empty sketch with parameter `k` (at least 8).
    pub fn new(k: usize) -> Kll {
        let mut s = Kll {
            k: k.max(8),
            levels: Vec::new(),
            size: 0,
            max_size: 0,
            n: 0,
            rng: 0x9e37_79b9_7f4a_7c15,
        };
        s.grow();
        s
    }

    /// Number of values pushed.
    pub fn count(&self) -> u64 {
        self.n
    }

    /// Number of values retained by the sketch.
    pub fn retained(&self) -> usize {
        self.size
    }

    fn capacity(&self, level: usize) -> usize {
        let depth = self.levels.len() - level - 1;
        let cap = (self.k as f64 * (2.0f64 / 3.0).powi(depth as i32)).ceil() as usize + 1;
        cap.max(2)
    }

    fn grow(&mut self) {
        self.levels.push(Vec::new());
        self.max_size = (0..self.levels.len()).map(|h| self.capacity(h)).sum();
    }

    fn coin(&mut self) -> bool {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x & 1 == 1
    }

    fn compress(&mut self) {
        for h in 0..self.levels.len() {
            if self.levels[h].len() >= self.capacity(h) {
                if h + 1 == self.levels.len() {
                    self.grow();
                }
                let mut buf = std::mem::take(&mut self.levels[h]);
                buf.sort_unstable_by(f64::total_cmp);
                // With an odd count the smallest item stays at this level.
                let start = buf.len() % 2;
                let offset = usize::from(self.coin());
                let promoted: Vec<f64> = buf[start..]
                    .iter()
                    .skip(offset)
                    .step_by(2)
                    .copied()
                    .collect();
                buf.truncate(start);
                self.levels[h] = buf;
                self.levels[h + 1].extend(promoted);
                self.size = self.levels.iter().map(Vec::len).sum();
                if self.size < self.max_size {
                    break;
                }
            }
        }
    }

    /// Adds a value.
    pub fn insert(&mut self, x: f64) {
        self.levels[0].push(x);
        self.size += 1;
        self.n += 1;
        if self.size >= self.max_size {
            self.compress();
        }
    }

    /// The approximate `q`-quantile (`0.0..=1.0`). `None` when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let mut items: Vec<(f64, u64)> = self
            .levels
            .iter()
            .enumerate()
            .flat_map(|(h, l)| l.iter().map(move |&x| (x, 1u64 << h)))
            .collect();
        if items.is_empty() {
            return None;
        }
        items.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let total: u64 = items.iter().map(|i| i.1).sum();
        let target = q.clamp(0.0, 1.0) * total as f64;
        let mut cum = 0u64;
        for &(x, w) in &items {
            cum += w;
            if cum as f64 >= target {
                return Some(x);
            }
        }
        items.last().map(|i| i.0)
    }

    /// Merges another sketch into this one.
    pub fn merge_sketch(&mut self, other: Kll) {
        while self.levels.len() < other.levels.len() {
            self.grow();
        }
        for (h, l) in other.levels.into_iter().enumerate() {
            self.levels[h].extend(l);
        }
        self.n += other.n;
        self.size = self.levels.iter().map(Vec::len).sum();
        while self.size >= self.max_size {
            self.compress();
        }
    }
}

/// Quantile accumulator: exact for a sample, KLL for a full profile.
#[derive(Clone, Debug, PartialEq)]
pub enum Quantiles {
    /// Exact (sample stats).
    Exact(ExactQuantiles),
    /// Approximate (full profile).
    Sketch(Kll),
}

impl Quantiles {
    /// Adds a value.
    pub fn insert(&mut self, x: f64) {
        match self {
            Quantiles::Exact(e) => e.insert(x),
            Quantiles::Sketch(s) => s.insert(x),
        }
    }

    /// The `q`-quantile, `None` when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        match self {
            Quantiles::Exact(e) => e.quantile(q),
            Quantiles::Sketch(s) => s.quantile(q),
        }
    }

    /// Whether values come from the sketch (shown with `~`).
    pub fn is_approximate(&self) -> bool {
        matches!(self, Quantiles::Sketch(_))
    }
}

/// The numeric value of a parsed field, if it is a number.
pub fn numeric(parsed: &Value) -> Option<f64> {
    match *parsed {
        Value::I64(i) => Some(i as f64),
        Value::F64(f) => Some(f),
        _ => None,
    }
}

impl Accumulate for Quantiles {
    fn push(&mut self, _v: &[u8], parsed: &Value) {
        if let Some(x) = numeric(parsed) {
            self.insert(x);
        }
    }

    /// Exact + exact stays exact; anything else becomes a sketch.
    fn merge(&mut self, other: Quantiles) {
        match (&mut *self, other) {
            (Quantiles::Exact(a), Quantiles::Exact(b)) => a.values.extend(b.values),
            (_, other) => {
                let into_sketch = |q: Quantiles| match q {
                    Quantiles::Sketch(s) => s,
                    Quantiles::Exact(e) => {
                        let mut s = Kll::default();
                        for x in e.values {
                            s.insert(x);
                        }
                        s
                    }
                };
                let mine = std::mem::replace(self, Quantiles::Exact(ExactQuantiles::new()));
                let mut s = into_sketch(mine);
                s.merge_sketch(into_sketch(other));
                *self = Quantiles::Sketch(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_quantiles() {
        let mut e = ExactQuantiles::new();
        assert_eq!(e.quantile(0.5), None);
        for x in [5.0, 1.0, 4.0, 2.0, 3.0] {
            e.insert(x);
        }
        assert_eq!(e.quantile(0.5), Some(3.0));
        assert_eq!(e.quantile(0.0), Some(1.0));
        assert_eq!(e.quantile(1.0), Some(5.0));
        assert_eq!(e.quantile(0.95), Some(4.8));
        e.insert(6.0);
        assert_eq!(e.quantile(0.5), Some(3.5));
    }

    fn shuffled(n: u64, seed: u64) -> Vec<f64> {
        let mut v: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let mut s = seed | 1;
        for i in (1..v.len()).rev() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            v.swap(i, (s % (i as u64 + 1)) as usize);
        }
        v
    }

    #[test]
    fn kll_rank_error_is_small() {
        let n = 200_000u64;
        let mut s = Kll::default();
        for x in shuffled(n, 7) {
            s.insert(x);
        }
        assert_eq!(s.count(), n);
        assert!(s.retained() < 2_000, "{}", s.retained());
        for q in [0.5, 0.95, 0.01, 0.99] {
            let got = s.quantile(q).unwrap();
            let rank_err = (got / n as f64 - q).abs();
            assert!(rank_err < 0.02, "q={q} got={got} err={rank_err}");
        }
    }

    #[test]
    fn kll_merge_stays_accurate() {
        let n = 100_000u64;
        let data = shuffled(n, 11);
        let mut parts: Vec<Kll> = data
            .chunks(7_000)
            .map(|c| {
                let mut s = Kll::default();
                c.iter().for_each(|&x| s.insert(x));
                s
            })
            .collect();
        let mut acc = parts.remove(0);
        for p in parts {
            acc.merge_sketch(p);
        }
        assert_eq!(acc.count(), n);
        for q in [0.5, 0.95] {
            let got = acc.quantile(q).unwrap();
            assert!((got / n as f64 - q).abs() < 0.02, "q={q} got={got}");
        }
    }
}
