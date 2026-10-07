//! Sample statistics for the inspector: null counts, HyperLogLog distinct
//! estimates, top-k values and numeric summaries (spec §7.2).
//!
//! Every accumulator implements [`Accumulate`], so the type-inference sample
//! (M3-01) and the full-file profile job (M5-04) feed the same code, and
//! per-chunk results can be merged.
//!
//! [`ColumnStats`] chooses exact accumulators for a sample ([`StatsMode::Sample`])
//! and mergeable sketches for a full scan ([`StatsMode::Full`]):
//!
//! | Part | Sample | Full profile |
//! |---|---|---|
//! | distinct | [`Hll`] (exact up to 1,024) | [`Hll`] |
//! | top values | exact counts | Space-Saving, 1,024 counters |
//! | p50 / p95 | exact | KLL, `k = 200` |
//! | min / max / mean | exact | exact |
//!
//! The sample stats are computed by [`crate::sample`] and delivered in
//! [`SampleResult`](crate::sample::SampleResult), which keeps the raw sample
//! values: after `set type`, [`SampleResult::stats_for`](crate::sample::SampleResult::stats_for)
//! recomputes them without file access.

pub mod hll;
pub mod profile;
pub mod quantile;
pub mod topk;

use std::fmt;

use unicode_width::UnicodeWidthStr;

pub use hll::Hll;
pub use quantile::{ExactQuantiles, Kll, Quantiles};
pub use topk::{SpaceSaving, TopEntry, TopK};

use crate::types::{ColType, NullSet, Value, format_date, format_datetime, parse_value};

/// Number of top values shown by the inspector.
pub const TOP_N: usize = 5;

/// A mergeable statistics accumulator.
pub trait Accumulate: Sized {
    /// Adds one field: its raw (unescaped) bytes and its value parsed as the
    /// column's type.
    fn push(&mut self, v: &[u8], parsed: &Value);
    /// Merges the accumulator of another part of the data into this one.
    fn merge(&mut self, other: Self);
}

// ---------------------------------------------------------------------------
// Mean, min / max
// ---------------------------------------------------------------------------

/// Mean of the numeric values: `i64` values are summed exactly as `i128`,
/// `f64` values with Neumaier (Kahan-Babuška) compensated summation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mean {
    int_sum: i128,
    sum: f64,
    comp: f64,
    count: u64,
}

impl Mean {
    fn add_f64(&mut self, x: f64) {
        let t = self.sum + x;
        if self.sum.abs() >= x.abs() {
            self.comp += (self.sum - t) + x;
        } else {
            self.comp += (x - t) + self.sum;
        }
        self.sum = t;
    }

    /// Number of numeric values seen.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The mean, `None` when no numeric value was seen.
    pub fn mean(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let total = self.int_sum as f64 + (self.sum + self.comp);
        Some(total / self.count as f64)
    }
}

impl Accumulate for Mean {
    fn push(&mut self, _v: &[u8], parsed: &Value) {
        match *parsed {
            Value::I64(i) => {
                self.int_sum += i128::from(i);
                self.count += 1;
            }
            Value::F64(f) => {
                self.add_f64(f);
                self.count += 1;
            }
            _ => {}
        }
    }

    fn merge(&mut self, other: Mean) {
        self.int_sum += other.int_sum;
        self.add_f64(other.sum);
        self.add_f64(other.comp);
        self.count += other.count;
    }
}

/// An ordered typed value: a min or max.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scalar {
    /// An `i64` value.
    I64(i64),
    /// An `f64` value.
    F64(f64),
    /// A date, days since 1970-01-01.
    Date(i32),
    /// A datetime, µs since the epoch (UTC).
    DateTime(i64),
}

impl Scalar {
    fn from_value(v: &Value) -> Option<Scalar> {
        match *v {
            Value::I64(i) => Some(Scalar::I64(i)),
            Value::F64(f) => Some(Scalar::F64(f)),
            Value::Date(d) => Some(Scalar::Date(d)),
            Value::DateTime(t) => Some(Scalar::DateTime(t)),
            _ => None,
        }
    }

    /// Compares two scalars of the same kind. Different kinds never meet in
    /// one column; they compare as numbers.
    fn cmp_same(&self, other: &Scalar) -> std::cmp::Ordering {
        match (self, other) {
            (Scalar::I64(a), Scalar::I64(b)) => a.cmp(b),
            (Scalar::Date(a), Scalar::Date(b)) => a.cmp(b),
            (Scalar::DateTime(a), Scalar::DateTime(b)) => a.cmp(b),
            (a, b) => a.as_f64().total_cmp(&b.as_f64()),
        }
    }

    /// The value as `f64` (dates as day / µs counts).
    pub fn as_f64(&self) -> f64 {
        match *self {
            Scalar::I64(i) => i as f64,
            Scalar::F64(f) => f,
            Scalar::Date(d) => f64::from(d),
            Scalar::DateTime(t) => t as f64,
        }
    }
}

impl fmt::Display for Scalar {
    /// Numbers plainly, dates as `YYYY-MM-DD`, datetimes as ISO 8601 UTC.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Scalar::I64(i) => write!(f, "{i}"),
            Scalar::F64(x) => write!(f, "{x}"),
            Scalar::Date(d) => f.write_str(&format_date(d)),
            Scalar::DateTime(t) => f.write_str(&format_datetime(t)),
        }
    }
}

/// Exact minimum and maximum of the typed values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MinMax {
    /// The smallest value seen.
    pub min: Option<Scalar>,
    /// The largest value seen.
    pub max: Option<Scalar>,
}

impl MinMax {
    fn add(&mut self, s: Scalar) {
        if self.min.is_none_or(|m| s.cmp_same(&m).is_lt()) {
            self.min = Some(s);
        }
        if self.max.is_none_or(|m| s.cmp_same(&m).is_gt()) {
            self.max = Some(s);
        }
    }
}

impl Accumulate for MinMax {
    fn push(&mut self, _v: &[u8], parsed: &Value) {
        if let Some(s) = Scalar::from_value(parsed) {
            self.add(s);
        }
    }

    fn merge(&mut self, other: MinMax) {
        if let Some(s) = other.min {
            self.add(s);
        }
        if let Some(s) = other.max {
            self.add(s);
        }
    }
}

/// min / max / mean / p50 / p95 of an `i64` or `f64` column (§7.2).
#[derive(Clone, Debug, PartialEq)]
pub struct NumericStats {
    /// Exact min and max.
    pub range: MinMax,
    /// Exact mean.
    pub mean: Mean,
    /// p50 / p95 source.
    pub quantiles: Quantiles,
}

impl NumericStats {
    /// Empty numeric stats. `mode` picks exact quantiles or a KLL sketch.
    pub fn new(mode: StatsMode) -> NumericStats {
        NumericStats {
            range: MinMax::default(),
            mean: Mean::default(),
            quantiles: match mode {
                StatsMode::Sample => Quantiles::Exact(ExactQuantiles::new()),
                StatsMode::Full => Quantiles::Sketch(Kll::default()),
            },
        }
    }

    /// Smallest value.
    pub fn min(&self) -> Option<Scalar> {
        self.range.min
    }

    /// Largest value.
    pub fn max(&self) -> Option<Scalar> {
        self.range.max
    }

    /// Mean.
    pub fn mean(&self) -> Option<f64> {
        self.mean.mean()
    }

    /// Median.
    pub fn p50(&self) -> Option<f64> {
        self.quantiles.quantile(0.5)
    }

    /// 95th percentile.
    pub fn p95(&self) -> Option<f64> {
        self.quantiles.quantile(0.95)
    }

    /// Whether p50 / p95 are approximate (shown with `~`).
    pub fn quantiles_approximate(&self) -> bool {
        self.quantiles.is_approximate()
    }
}

impl Accumulate for NumericStats {
    fn push(&mut self, v: &[u8], parsed: &Value) {
        self.range.push(v, parsed);
        self.mean.push(v, parsed);
        self.quantiles.push(v, parsed);
    }

    fn merge(&mut self, other: NumericStats) {
        self.range.merge(other.range);
        self.mean.merge(other.mean);
        self.quantiles.merge(other.quantiles);
    }
}

// ---------------------------------------------------------------------------
// Column stats
// ---------------------------------------------------------------------------

/// Which accumulators [`ColumnStats`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatsMode {
    /// The type-inference sample: exact top-k and quantiles.
    Sample,
    /// A full-file profile: Space-Saving top-k and a KLL sketch.
    Full,
}

/// Where stats come from; drives the inspector label (§7.2, D6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatsSource {
    /// Computed from a sample of `rows` rows.
    Sample {
        /// Rows in the sample.
        rows: u64,
    },
    /// Computed from every row (after `:profile`).
    AllRows,
}

impl StatsSource {
    /// `sample 20k rows` (the real sample size, D6) or `all rows`.
    pub fn label(&self) -> String {
        match self {
            StatsSource::Sample { rows } => {
                format!("sample {} rows", crate::size::format_count_compact(*rows))
            }
            StatsSource::AllRows => "all rows".to_owned(),
        }
    }
}

/// Per-column statistics shown by the inspector (spec §7.2).
#[derive(Clone, Debug)]
pub struct ColumnStats {
    /// Values observed.
    pub rows_seen: u64,
    /// Nulls per [`NullSet`]. Shown as a percentage with 1 decimal.
    pub nulls: u64,
    /// Distinct non-null values; always shown with a `~` prefix.
    pub distinct: Hll,
    /// Display width of the widest value seen (nulls excluded).
    pub max_width: u16,
    /// Most frequent non-null values.
    pub top: TopK,
    /// min / max / mean / p50 / p95, for `i64` and `f64` columns only.
    pub numeric: Option<NumericStats>,
    /// min / max for `date` and `datetime` columns (shown formatted).
    pub temporal: Option<MinMax>,
    /// Where the stats come from.
    pub source: StatsSource,
    /// The type the stats were computed for; a different effective type means
    /// they are stale.
    pub for_type: ColType,
}

impl ColumnStats {
    /// Empty stats for a column of type `ty`.
    pub fn new(ty: ColType, mode: StatsMode) -> ColumnStats {
        ColumnStats {
            rows_seen: 0,
            nulls: 0,
            distinct: Hll::new(),
            max_width: 0,
            top: match mode {
                StatsMode::Sample => TopK::exact(),
                StatsMode::Full => TopK::space_saving(),
            },
            numeric: ty.is_numeric().then(|| NumericStats::new(mode)),
            temporal: ty.is_temporal().then(MinMax::default),
            source: match mode {
                StatsMode::Sample => StatsSource::Sample { rows: 0 },
                StatsMode::Full => StatsSource::AllRows,
            },
            for_type: ty,
        }
    }

    /// Computes sample stats from in-memory values (one per sampled row). Used
    /// for recomputing after `set type` from the cached sample, with no file
    /// access.
    pub fn from_values<'a, I>(ty: ColType, values: I, nulls: &NullSet) -> ColumnStats
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let mut s = ColumnStats::new(ty, StatsMode::Sample);
        for v in values {
            s.push_raw(v, nulls);
        }
        s
    }

    /// Parses `v` as [`ColumnStats::for_type`] and pushes it.
    pub fn push_raw(&mut self, v: &[u8], nulls: &NullSet) {
        let parsed = parse_value(self.for_type, v, nulls);
        self.push(v, &parsed);
    }

    /// Non-null values seen.
    pub fn non_null(&self) -> u64 {
        self.rows_seen - self.nulls
    }

    /// Null percentage (0–100), `None` before any row.
    pub fn null_percent(&self) -> Option<f64> {
        (self.rows_seen > 0).then(|| self.nulls as f64 * 100.0 / self.rows_seen as f64)
    }

    /// The top [`TOP_N`] values.
    pub fn top5(&self) -> Vec<TopEntry> {
        self.top.top(TOP_N)
    }

    /// `rows_seen − nulls − Σ top5`. With approximate (Space-Saving) counts,
    /// this saturates at 0.
    pub fn other(&self) -> u64 {
        let top: u64 = self.top5().iter().map(|e| e.count).sum();
        self.non_null().saturating_sub(top)
    }

    /// Whether these stats were computed for a type other than `ty`.
    pub fn is_stale_for(&self, ty: ColType) -> bool {
        self.for_type != ty
    }

    /// `sample 20k rows` or `all rows`.
    pub fn label(&self) -> String {
        self.source.label()
    }
}

impl Accumulate for ColumnStats {
    /// Pushes one field. `parsed` must be `v` parsed as `for_type`.
    ///
    /// The value is decoded (lossy UTF-8; the parse layer already transcoded
    /// other encodings) only to measure `max_width`. Everything else works on
    /// the raw bytes.
    fn push(&mut self, v: &[u8], parsed: &Value) {
        self.rows_seen += 1;
        if let StatsSource::Sample { rows } = &mut self.source {
            *rows += 1;
        }
        if matches!(parsed, Value::Null) {
            self.nulls += 1;
            return;
        }
        let width = String::from_utf8_lossy(v).width();
        self.max_width = self.max_width.max(width.min(usize::from(u16::MAX)) as u16);
        self.distinct.push(v, parsed);
        self.top.push(v, parsed);
        if let Some(n) = &mut self.numeric {
            n.push(v, parsed);
        }
        if let Some(t) = &mut self.temporal {
            t.push(v, parsed);
        }
    }

    /// Merges stats of another part of the data. Both sides must be for the
    /// same type; if not, `other`'s numeric and temporal parts are dropped.
    fn merge(&mut self, other: ColumnStats) {
        self.rows_seen += other.rows_seen;
        self.nulls += other.nulls;
        self.max_width = self.max_width.max(other.max_width);
        self.distinct.merge(other.distinct);
        self.top.merge(other.top);
        if let (Some(a), Some(b)) = (&mut self.numeric, other.numeric) {
            a.merge(b);
        }
        if let (Some(a), Some(b)) = (&mut self.temporal, other.temporal) {
            a.merge(b);
        }
        self.source = match (self.source, other.source) {
            (StatsSource::Sample { rows: a }, StatsSource::Sample { rows: b }) => {
                StatsSource::Sample { rows: a + b }
            }
            _ => StatsSource::AllRows,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(
            StatsSource::Sample { rows: 20_000 }.label(),
            "sample 20k rows"
        );
        assert_eq!(StatsSource::Sample { rows: 10 }.label(), "sample 10 rows");
        assert_eq!(StatsSource::AllRows.label(), "all rows");
    }

    #[test]
    fn mean_is_exact_for_integers() {
        let mut m = Mean::default();
        for i in [i64::MAX, i64::MAX, -1] {
            m.push(b"", &Value::I64(i));
        }
        let want = (2 * i128::from(i64::MAX) - 1) as f64 / 3.0;
        assert_eq!(m.mean(), Some(want));
    }

    #[test]
    fn kahan_keeps_small_terms() {
        let mut m = Mean::default();
        m.push(b"", &Value::F64(1e16));
        for _ in 0..10_000 {
            m.push(b"", &Value::F64(1.0));
        }
        m.push(b"", &Value::F64(-1e16));
        assert_eq!(m.mean(), Some(10_000.0 / 10_002.0));
    }

    #[test]
    fn min_max_temporal() {
        let n = NullSet::default();
        let s = ColumnStats::from_values(
            ColType::Date,
            ["2024-02-29", "", "1999-01-01", "bad", "2030-12-31"].map(str::as_bytes),
            &n,
        );
        let t = s.temporal.as_ref().unwrap();
        assert_eq!(t.min.unwrap().to_string(), "1999-01-01");
        assert_eq!(t.max.unwrap().to_string(), "2030-12-31");
        assert!(s.numeric.is_none());
        assert_eq!(s.nulls, 1);
        assert_eq!(s.rows_seen, 5);
    }

    #[test]
    fn max_width_uses_display_width() {
        let s = ColumnStats::from_values(
            ColType::Str,
            ["abc", "日本語", "NULL", "a\u{301}"].map(str::as_bytes),
            &NullSet::default(),
        );
        assert_eq!(s.max_width, 6);
    }

    #[test]
    fn stale_after_type_change() {
        let s = ColumnStats::new(ColType::I64, StatsMode::Sample);
        assert!(!s.is_stale_for(ColType::I64));
        assert!(s.is_stale_for(ColType::Str));
    }
}
