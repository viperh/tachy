//! Per-column metadata: names, inferred and overridden types, sample stats and
//! the sample width histogram (spec §7.1, §7.2, §11.3).
//!
//! Display order, visibility and widths are per tab, in `ColumnLayout`
//! (M1-05); they are not stored here.

pub use crate::dialect::ColumnName;
use crate::{stats::ColumnStats, types::ColType};

/// Number of exact width buckets; wider values share one overflow bucket.
pub const WIDTH_BUCKETS: usize = 256;

/// Histogram of value display widths over the sample (M3-04): one bucket per
/// width 0..=255 plus a 256+ bucket. Nulls count as width 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WidthHistogram {
    counts: Box<[u64]>,
    total: u64,
}

impl Default for WidthHistogram {
    fn default() -> WidthHistogram {
        WidthHistogram {
            counts: vec![0; WIDTH_BUCKETS + 1].into_boxed_slice(),
            total: 0,
        }
    }
}

impl WidthHistogram {
    /// Records one value of display width `width`.
    pub fn push(&mut self, width: usize) {
        self.counts[width.min(WIDTH_BUCKETS)] += 1;
        self.total += 1;
    }

    /// Number of values recorded.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Adds another histogram's counts.
    pub fn merge(&mut self, other: &WidthHistogram) {
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a += b;
        }
        self.total += other.total;
    }

    /// The smallest width `w` whose cumulative count is ≥ 95 % of the total;
    /// 256 stands for "256 or wider". `None` when empty.
    pub fn p95(&self) -> Option<usize> {
        if self.total == 0 {
            return None;
        }
        let mut cum = 0u64;
        for (w, &c) in self.counts.iter().enumerate() {
            cum += c;
            if cum * 100 >= self.total * 95 {
                return Some(w);
            }
        }
        Some(WIDTH_BUCKETS)
    }
}

/// Metadata for one column of a tab (spec §7.1).
#[derive(Clone, Debug)]
pub struct ColumnMeta {
    /// Display + query name (M1-02).
    pub name: ColumnName,
    /// Position in the record; `_extraN` columns continue after the header.
    pub source_index: usize,
    /// Whether this is a synthetic `_extraN` column (M1-04).
    pub synthetic: bool,
    /// The type inferred from the sample.
    pub inferred: ColType,
    /// The type set with `set type`, which wins over `inferred`.
    pub type_override: Option<ColType>,
    /// Sample or profile statistics (M3-02).
    pub stats: Option<ColumnStats>,
    /// Sample width histogram (M3-04).
    pub sample_widths: WidthHistogram,
}

impl ColumnMeta {
    /// A column with no sample yet: type `str`, no stats.
    pub fn new(name: ColumnName, source_index: usize, synthetic: bool) -> ColumnMeta {
        ColumnMeta {
            name,
            source_index,
            synthetic,
            inferred: ColType::Str,
            type_override: None,
            stats: None,
            sample_widths: WidthHistogram::default(),
        }
    }

    /// The effective type: the override, or else the inferred type.
    pub fn ty(&self) -> ColType {
        self.type_override.unwrap_or(self.inferred)
    }

    /// Records a (re-)inferred type. Never touches `type_override`, so a
    /// `set type` survives phase-2 re-inference.
    pub fn set_inferred(&mut self, ty: ColType) {
        self.inferred = ty;
    }

    /// Whether the stats were computed for another type than [`ColumnMeta::ty`]
    /// (the inspector then shows `stats for <old type>`).
    pub fn stats_stale(&self) -> bool {
        self.stats
            .as_ref()
            .is_some_and(|s| s.is_stale_for(self.ty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::StatsMode;

    fn name(n: &str) -> ColumnName {
        ColumnName {
            display: n.to_owned(),
            query: n.to_owned(),
        }
    }

    #[test]
    fn override_survives_reinference() {
        let mut c = ColumnMeta::new(name("flag"), 0, false);
        c.set_inferred(ColType::Bool);
        assert_eq!(c.ty(), ColType::Bool);
        c.type_override = Some(ColType::I64);
        assert_eq!(c.ty(), ColType::I64);
        // Phase 2 infers again.
        c.set_inferred(ColType::Str);
        assert_eq!(c.inferred, ColType::Str);
        assert_eq!(c.ty(), ColType::I64);
    }

    #[test]
    fn stale_stats_after_override() {
        let mut c = ColumnMeta::new(name("x"), 0, false);
        c.set_inferred(ColType::I64);
        c.stats = Some(ColumnStats::new(ColType::I64, StatsMode::Sample));
        assert!(!c.stats_stale());
        c.type_override = Some(ColType::Str);
        assert!(c.stats_stale());
    }

    #[test]
    fn width_p95() {
        let mut h = WidthHistogram::default();
        assert_eq!(h.p95(), None);
        for w in 1..=100 {
            h.push(w);
        }
        assert_eq!(h.p95(), Some(95));
        h.push(10_000);
        assert_eq!(h.total(), 101);
        let mut g = WidthHistogram::default();
        g.merge(&h);
        assert_eq!(g, h);
    }
}
