//! Sample collection for type inference and inspector statistics (spec §7.1,
//! §7.2; M3-01, M3-02).
//!
//! Two phases:
//!
//! 1. [`sample_head`], on open: the first [`HEAD_ROWS`] rows, parsed
//!    sequentially from `data_start` in one `exec.run` (no index needed).
//! 2. [`sample_spread`], once the index is ready: [`SPREAD_ROWS`] more rows
//!    spread evenly over the rest of the file, resolved with
//!    [`RowIndex::offset_of`] in parallel slices, combined with the phase-1
//!    rows (20,000 rows in total).
//!
//! Spread rows are taken from the rows **after** the head sample
//! (`head + k * (total − head) / 10,000`, deduplicated), so no row is counted
//! twice; when the file has at most 20,000 rows every row is sampled once.
//! A file whose head sample reached EOF gets no spread rows.
//!
//! Each phase returns a [`SampleResult`]: per column (by field position, so
//! `per_column[i]` is the column with `source_index == i`, `_extraN`
//! included) the inferred type, the sample [`ColumnStats`] and the
//! [`WidthHistogram`]. It also keeps the **raw sample values** (capped at
//! [`MAX_ROW_BYTES`] per row), so stats for another type (`set type`) are
//! recomputed with [`SampleResult::stats_for`] without touching the file. A
//! `SampleResult` holds no reference to the [`Source`].

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

use crate::{
    Error,
    column::{ColumnMeta, WidthHistogram},
    dialect::Encoding,
    exec::Executor,
    index::RowIndex,
    parse::{ParseOutcome, RecordParser, RecordRanges, decode_field},
    source::Source,
    stats::{Accumulate, ColumnStats, StatsMode},
    types::{ColType, NullSet, TypeInference, Value, parse_value},
};

/// Rows of the phase-1 (head) sample.
pub const HEAD_ROWS: u64 = 10_000;
/// Rows added by the phase-2 (spread) sample.
pub const SPREAD_ROWS: u64 = 10_000;
/// Raw bytes kept per sampled row; longer rows have their last values
/// truncated in the cache (stats and inference use the cached bytes).
pub const MAX_ROW_BYTES: usize = 64 << 10;

/// Rows between cancellation checks.
const CHECK_EVERY: usize = 256;

/// Which phase produced a [`SampleResult`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplePhase {
    /// The first [`HEAD_ROWS`] rows.
    Head,
    /// Head plus spread rows.
    Spread,
}

/// The cached raw values of one column, one entry per sampled row.
#[derive(Clone, Debug, Default)]
pub struct SampleValues {
    data: Vec<u8>,
    /// `(start, len)` into `data`, or `None` for a missing field (short row).
    spans: Vec<Option<(usize, u32)>>,
}

impl SampleValues {
    /// Number of sampled rows.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// No rows.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The value of sampled row `i` (unescaped bytes), `None` when the row
    /// has no such field.
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let (start, len) = (*self.spans.get(i)?)?;
        Some(&self.data[start..start + len as usize])
    }

    /// All values in sample order.
    pub fn iter(&self) -> impl Iterator<Item = Option<&[u8]>> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    /// Bytes held by the cache.
    pub fn memory_bytes(&self) -> usize {
        self.data.capacity() + self.spans.capacity() * std::mem::size_of::<Option<(usize, u32)>>()
    }

    fn push(&mut self, v: Option<&[u8]>) {
        self.spans.push(v.map(|v| {
            let start = self.data.len();
            self.data.extend_from_slice(v);
            (start, v.len() as u32)
        }));
    }

    fn push_missing(&mut self, n: usize) {
        self.spans.extend(std::iter::repeat_n(None, n));
    }

    fn append(&mut self, other: &SampleValues) {
        let base = self.data.len();
        self.data.extend_from_slice(&other.data);
        self.spans
            .extend(other.spans.iter().map(|s| s.map(|(st, l)| (st + base, l))));
    }
}

/// The sample of one column.
#[derive(Clone, Debug)]
pub struct ColumnSample {
    /// Type inferred from the sample (§7.1).
    pub inferred: ColType,
    /// Sample stats for `inferred` (§7.2), labelled `sample N rows`.
    pub stats: ColumnStats,
    /// Display widths of the sampled values (M3-04). Nulls and missing
    /// fields count as width 0.
    pub widths: WidthHistogram,
    /// The cached raw values.
    pub values: SampleValues,
}

/// What a sample phase delivers (`Msg::SampleReady`).
#[derive(Clone, Debug)]
pub struct SampleResult {
    /// The phase that produced it.
    pub phase: SamplePhase,
    /// Rows in the sample.
    pub rows_sampled: u64,
    /// Rows from the head (phase 1) part.
    pub head_rows: u64,
    /// The head sample reached the end of the file: it holds every row.
    pub reached_eof: bool,
    /// Per field position (`source_index`).
    pub per_column: Vec<ColumnSample>,
}

impl SampleResult {
    /// Sample stats of column `col` (field position) computed for `ty` from
    /// the cached values. **No file access**: used after `set type`.
    pub fn stats_for(&self, col: usize, ty: ColType, nulls: &NullSet) -> Option<ColumnStats> {
        let c = self.per_column.get(col)?;
        Some(stats_from_values(&c.values, ty, nulls))
    }

    /// Applies the sample to a tab's columns (matched by `source_index`):
    /// sets the inferred type (never touching `type_override`), the stats for
    /// the **effective** type (recomputed from the cache when an override is
    /// set) and the width histogram.
    pub fn apply(&self, cols: &mut [ColumnMeta], nulls: &NullSet) {
        for meta in cols {
            let Some(c) = self.per_column.get(meta.source_index) else {
                continue;
            };
            meta.set_inferred(c.inferred);
            meta.stats = Some(if meta.ty() == c.inferred {
                c.stats.clone()
            } else {
                stats_from_values(&c.values, meta.ty(), nulls)
            });
            meta.sample_widths = c.widths.clone();
        }
    }

    /// Bytes held by the cached sample values.
    pub fn memory_bytes(&self) -> usize {
        self.per_column
            .iter()
            .map(|c| c.values.memory_bytes())
            .sum()
    }
}

fn stats_from_values(values: &SampleValues, ty: ColType, nulls: &NullSet) -> ColumnStats {
    let mut s = ColumnStats::new(ty, StatsMode::Sample);
    for v in values.iter() {
        match v {
            Some(v) => {
                let parsed = parse_value(ty, v, nulls);
                s.push(v, &parsed);
            }
            None => s.push(&[], &Value::Null),
        }
    }
    s
}

/// Raw values collected row by row, in columns.
#[derive(Clone, Debug, Default)]
struct Collector {
    cols: Vec<SampleValues>,
    rows: usize,
}

impl Collector {
    fn with_width(width: usize) -> Collector {
        Collector {
            cols: vec![SampleValues::default(); width],
            rows: 0,
        }
    }

    fn widen(&mut self, width: usize) {
        while self.cols.len() < width {
            let mut v = SampleValues::default();
            v.push_missing(self.rows);
            self.cols.push(v);
        }
    }

    fn push_row(
        &mut self,
        bytes: &[u8],
        rec: &RecordRanges,
        parser: &RecordParser,
        scratch: &mut Vec<u8>,
    ) {
        self.widen(rec.fields.len());
        let mut budget = MAX_ROW_BYTES;
        for (i, col) in self.cols.iter_mut().enumerate() {
            if i < rec.fields.len() {
                let v = parser.field_value(bytes, rec, i, scratch);
                let v = &v[..v.len().min(budget)];
                budget -= v.len();
                col.push(Some(v));
            } else {
                col.push(None);
            }
        }
        self.rows += 1;
    }

    fn append(&mut self, other: &Collector) {
        self.widen(other.cols.len());
        for (i, col) in self.cols.iter_mut().enumerate() {
            match other.cols.get(i) {
                Some(o) => col.append(o),
                None => col.push_missing(other.rows),
            }
        }
        self.rows += other.rows;
    }

    fn finish(
        self,
        phase: SamplePhase,
        head_rows: u64,
        reached_eof: bool,
        nulls: &NullSet,
        enc: Encoding,
    ) -> SampleResult {
        let rows = self.rows as u64;
        let per_column = self
            .cols
            .into_iter()
            .map(|values| {
                let mut inference = TypeInference::new();
                let mut widths = WidthHistogram::default();
                for v in values.iter() {
                    match v {
                        Some(v) => {
                            inference.push(v, nulls);
                            widths.push(if nulls.is_null(v) {
                                0
                            } else {
                                decode_field(v, enc).width()
                            });
                        }
                        None => widths.push(0),
                    }
                }
                let inferred = inference.infer();
                ColumnSample {
                    inferred,
                    stats: stats_from_values(&values, inferred, nulls),
                    widths,
                    values,
                }
            })
            .collect();
        SampleResult {
            phase,
            rows_sampled: rows,
            head_rows,
            reached_eof,
            per_column,
        }
    }
}

/// Phase 1: the first [`HEAD_ROWS`] rows, parsed sequentially from
/// `data_start` on the blocking pool. Returns [`Error::Cancelled`] if
/// `cancel` fires.
pub async fn sample_head(
    src: Arc<Source>,
    exec: &Executor,
    nulls: NullSet,
    cancel: CancellationToken,
) -> Result<SampleResult, Error> {
    exec.run(move || {
        let bytes = src.bytes();
        let mut parser = RecordParser::new(src.dialect());
        let mut rec = RecordRanges::default();
        let mut scratch = Vec::new();
        let mut c = Collector::with_width(src.width());
        let mut pos = src.data_start();
        let mut eof = false;
        while (c.rows as u64) < HEAD_ROWS {
            if c.rows.is_multiple_of(CHECK_EVERY) && cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            match parser.parse_at(bytes, pos, &mut rec) {
                ParseOutcome::Record { next } => {
                    c.push_row(bytes, &rec, &parser, &mut scratch);
                    pos = next;
                }
                ParseOutcome::UnterminatedQuote { .. } => {
                    c.push_row(bytes, &rec, &parser, &mut scratch);
                    eof = true;
                    break;
                }
                ParseOutcome::Eof => {
                    eof = true;
                    break;
                }
            }
        }
        if !eof {
            // Exactly HEAD_ROWS rows: EOF if nothing follows.
            eof = parser.next_record(bytes, pos).is_none();
        }
        let head = c.rows as u64;
        Ok(c.finish(SamplePhase::Head, head, eof, &nulls, src.dialect().encoding))
    })
    .await
}

/// The phase-2 row ids: [`SPREAD_ROWS`] ids spread evenly over
/// `head .. total`, deduplicated (all of them when fewer remain).
pub fn spread_row_ids(head: u64, total: u64) -> Vec<u64> {
    if total <= head {
        return Vec::new();
    }
    let rest = total - head;
    if rest <= SPREAD_ROWS {
        return (head..total).collect();
    }
    let mut ids: Vec<u64> = (0..SPREAD_ROWS)
        .map(|k| head + (u128::from(k) * u128::from(rest) / u128::from(SPREAD_ROWS)) as u64)
        .collect();
    ids.dedup();
    ids
}

/// Phase 2: adds [`SPREAD_ROWS`] rows spread across the file (see
/// [`spread_row_ids`]) to the head sample and infers again on the combined
/// rows. Rows are resolved with [`RowIndex::offset_of`] in `exec.threads()`
/// parallel slices; only rows below `index.indexed_rows()` are used, so call
/// it once the index is complete.
pub async fn sample_spread(
    src: Arc<Source>,
    index: Arc<RowIndex>,
    exec: &Executor,
    nulls: NullSet,
    head: Arc<SampleResult>,
    cancel: CancellationToken,
) -> Result<SampleResult, Error> {
    let ids = if head.reached_eof {
        Vec::new()
    } else {
        spread_row_ids(head.head_rows, index.indexed_rows())
    };
    let slices = exec.threads().max(1);
    let per = ids.len().div_ceil(slices).max(1);
    let ids = Arc::new(ids);
    let mut tasks = Vec::new();
    for k in 0..slices {
        let range = (k * per).min(ids.len())..((k + 1) * per).min(ids.len());
        if range.is_empty() {
            continue;
        }
        let (src, index, ids, cancel, exec) = (
            Arc::clone(&src),
            Arc::clone(&index),
            Arc::clone(&ids),
            cancel.clone(),
            exec.clone(),
        );
        tasks.push(tokio::spawn(async move {
            exec.run(move || {
                let bytes = src.bytes();
                let mut parser = RecordParser::new(src.dialect());
                let mut rec = RecordRanges::default();
                let mut scratch = Vec::new();
                let mut c = Collector::default();
                for (n, &row) in ids[range].iter().enumerate() {
                    if n.is_multiple_of(CHECK_EVERY) && cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let Some(offset) = index.offset_of(row, &src, &mut parser) else {
                        continue;
                    };
                    if !matches!(parser.parse_at(bytes, offset, &mut rec), ParseOutcome::Eof) {
                        c.push_row(bytes, &rec, &parser, &mut scratch);
                    }
                }
                Ok(c)
            })
            .await
        }));
    }
    // Rebuild the head part from its cached values.
    let mut all = Collector {
        cols: head.per_column.iter().map(|c| c.values.clone()).collect(),
        rows: head.head_rows as usize,
    };
    for t in tasks {
        let part = match t.await {
            Ok(r) => r?,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => return Err(Error::Cancelled),
        };
        all.append(&part);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let enc = src.dialect().encoding;
    let (head_rows, eof) = (head.head_rows, head.reached_eof);
    Ok(exec
        .run(move || all.finish(SamplePhase::Spread, head_rows, eof, &nulls, enc))
        .await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_ids() {
        assert!(spread_row_ids(10_000, 10_000).is_empty());
        assert_eq!(spread_row_ids(10, 15), vec![10, 11, 12, 13, 14]);
        let ids = spread_row_ids(10_000, 1_000_000);
        assert_eq!(ids.len(), 10_000);
        assert_eq!(ids[0], 10_000);
        assert!(*ids.last().unwrap() < 1_000_000);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        let ids = spread_row_ids(10_000, 20_001);
        assert_eq!(ids.len(), 10_000);
    }

    #[test]
    fn values_round_trip() {
        let mut v = SampleValues::default();
        v.push(Some(b"ab"));
        v.push(None);
        v.push(Some(b""));
        let mut w = SampleValues::default();
        w.push(Some(b"xyz"));
        v.append(&w);
        let got: Vec<_> = v.iter().collect();
        assert_eq!(
            got,
            vec![Some(&b"ab"[..]), None, Some(&b""[..]), Some(&b"xyz"[..])]
        );
    }
}
