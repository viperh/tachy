//! The profile job: exact full-file statistics for one column or all columns
//! (spec §7.2, §10.1; M5-04).
//!
//! [`run_profile`] scans the **whole file** (not the active view). It needs
//! the total row count and checkpoint-aligned chunks, so it first waits for
//! the index to complete (the UI shows `waiting for index` while
//! `!index.is_complete()`). Then checkpoint-aligned chunks of about
//! [`PROFILE_CHUNK_BYTES`] run through [`Executor::run`]; each keeps its own
//! [`ColumnStats`] (in [`StatsMode::Full`]) for the profiled columns, and only
//! those columns' fields are unescaped, parsed and decoded (for
//! `max_width`).
//!
//! Chunk results are merged with [`Accumulate::merge`]. Every accumulator is
//! commutative, but floating-point sums, the KLL sketch and Space-Saving
//! evictions depend on the merge order, so results are merged **in chunk
//! order** (through a [`ReorderBuffer`]): the chunk boundaries depend only on
//! the file and the index, so the result is identical whatever `--threads`
//! is.
//!
//! What is exact: row count, nulls, `max_width`, min / max / mean, and the
//! top values while the column has at most `capacity` distinct values.
//! Distinct stays approximate (HyperLogLog, `~`), p50 / p95 come from a KLL
//! sketch (`~`), and top values above the capacity are Space-Saving counts
//! (`~`).
//!
//! # Memory (§10.4)
//!
//! A Space-Saving counter costs about [`COUNTER_BYTES`] (100 B: the key, up
//! to 256 bytes but usually short, plus the hash-map slot and heap entry).
//! With `C` profiled columns and `W` chunk accumulators alive at once (in
//! flight or waiting in the reorder buffer) and a budget `B` (the job's
//! 256 MiB grant):
//!
//! - `W = clamp(B / (C × 64 × 100 B), 1, threads × 2)`: fewer chunks in
//!   flight when even the smallest sketches would not fit;
//! - `capacity = clamp(B / (C × W × 100 B), 64, 1024)` ([`topk_capacity`]).

use std::{sync::Arc, time::Duration};

use tokio::task::JoinSet;

use super::{Accumulate, ColumnStats, StatsMode, StatsSource, TopK, topk::SpaceSaving};
use crate::{
    column::ColumnMeta,
    exec::Executor,
    filter::{RangePlanner, ReorderBuffer, RowRange, Ticker},
    index::RowIndex,
    jobs::{JobControl, JobError, PROFILE_BUDGET, RowsProgress},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    source::Source,
    types::{ColType, NullSet, Value, parse_value},
};

/// Nominal chunk size (§7.2): 64 MiB.
pub const PROFILE_CHUNK_BYTES: u64 = 64 << 20;
/// Estimated memory of one Space-Saving counter.
pub const COUNTER_BYTES: u64 = 100;
/// Smallest Space-Saving capacity.
pub const MIN_CAPACITY: usize = 64;
/// Largest Space-Saving capacity (`topk::SPACE_SAVING_CAPACITY`).
pub const MAX_CAPACITY: usize = super::topk::SPACE_SAVING_CAPACITY;

/// Space-Saving capacity for `columns` profiled columns with `in_flight`
/// chunk accumulators: `clamp(budget / (C × W × 100 B), 64, 1024)`.
pub fn topk_capacity(budget: u64, columns: usize, in_flight: usize) -> usize {
    let per = (columns.max(1) as u64)
        .saturating_mul(in_flight.max(1) as u64)
        .saturating_mul(COUNTER_BYTES);
    usize::try_from(budget / per)
        .unwrap_or(usize::MAX)
        .clamp(MIN_CAPACITY, MAX_CAPACITY)
}

/// Chunk accumulators alive at once: `clamp(budget / (C × 64 × 100 B), 1,
/// threads × 2)`.
pub fn in_flight_chunks(budget: u64, columns: usize, threads: usize) -> usize {
    let per = (columns.max(1) as u64) * MIN_CAPACITY as u64 * COUNTER_BYTES;
    usize::try_from(budget / per)
        .unwrap_or(usize::MAX)
        .clamp(1, threads.max(1) * 2)
}

/// One profiled column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileColumn {
    /// Index into the tab's columns.
    pub column: usize,
    /// Field position (`ColumnMeta::source_index`).
    pub field: usize,
    /// The effective type the stats are computed for.
    pub ty: ColType,
}

impl ProfileColumn {
    /// Column `column` of a tab, with its current effective type.
    pub fn from_meta(column: usize, meta: &ColumnMeta) -> ProfileColumn {
        ProfileColumn {
            column,
            field: meta.source_index,
            ty: meta.ty(),
        }
    }

    /// Every column of a tab (`profile all`).
    pub fn all(cols: &[ColumnMeta]) -> Vec<ProfileColumn> {
        cols.iter()
            .enumerate()
            .map(|(i, m)| ProfileColumn::from_meta(i, m))
            .collect()
    }
}

/// Tuning of [`run_profile`].
#[derive(Clone, Debug)]
pub struct ProfileOptions {
    /// The job's memory grant (default [`PROFILE_BUDGET`]).
    pub budget: u64,
    /// Nominal chunk size (default [`PROFILE_CHUNK_BYTES`]).
    pub chunk_bytes: u64,
    /// Wait between checks of the index while it builds.
    pub poll: Duration,
}

impl Default for ProfileOptions {
    fn default() -> ProfileOptions {
        ProfileOptions {
            budget: PROFILE_BUDGET,
            chunk_bytes: PROFILE_CHUNK_BYTES,
            poll: Duration::from_millis(100),
        }
    }
}

/// What a finished profile delivers (`ProfileDone`).
#[derive(Clone, Debug)]
pub struct ProfileResult {
    /// `(column index, stats)` for every profiled column, in request order.
    /// The stats are labelled [`StatsSource::AllRows`].
    pub columns: Vec<(usize, ColumnStats)>,
    /// Rows scanned (the file's total).
    pub rows: u64,
    /// The Space-Saving capacity used.
    pub capacity: usize,
}

impl ProfileResult {
    /// Replaces the stats of the profiled columns in `cols`. A column whose
    /// effective type changed since the job started keeps its stats (the UI
    /// cancels the job on `set type`; this is the safety net).
    pub fn apply(&self, cols: &mut [ColumnMeta]) {
        for (i, stats) in &self.columns {
            if let Some(meta) = cols.get_mut(*i)
                && meta.ty() == stats.for_type
            {
                meta.stats = Some(stats.clone());
            }
        }
    }
}

fn empty_stats(ty: ColType, capacity: usize) -> ColumnStats {
    let mut s = ColumnStats::new(ty, StatsMode::Full);
    s.top = TopK::SpaceSaving(SpaceSaving::with_capacity(capacity));
    s
}

fn profile_chunk(
    src: &Source,
    cols: &[ProfileColumn],
    nulls: &NullSet,
    capacity: usize,
    range: RowRange,
    ctl: &JobControl,
    progress: &RowsProgress,
) -> Result<Vec<ColumnStats>, JobError> {
    ctl.check()?;
    let bytes = src.bytes();
    let mut stats: Vec<ColumnStats> = cols.iter().map(|c| empty_stats(c.ty, capacity)).collect();
    let mut parser = RecordParser::new(src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = Vec::new();
    let mut ticker = Ticker::new(ctl);
    let mut pos = range.start;
    let mut unreported = 0u64;
    for _ in 0..range.rows {
        let next = match parser.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
            ParseOutcome::Eof => break,
        };
        for (c, s) in cols.iter().zip(stats.iter_mut()) {
            if c.field < rec.fields.len() {
                let v = parser.field_value(bytes, &rec, c.field, &mut scratch);
                let parsed = parse_value(c.ty, v, nulls);
                s.push(v, &parsed);
            } else {
                // A short ragged row: the missing cell is null (§6.4).
                s.push(&[], &Value::Null);
            }
        }
        unreported += 1;
        if unreported == 1024 {
            progress.add(unreported);
            unreported = 0;
        }
        ticker.tick(next - pos)?;
        pos = next;
    }
    progress.add(unreported);
    Ok(stats)
}

/// Runs a profile job (M5-04). See the module docs.
///
/// - Waits for the index to complete first (polling, cancellable).
/// - `progress` counts rows; its total is set to `total_rows` once known.
/// - Cancellation returns [`JobError::Cancelled`]; the caller keeps the
///   previous (sample) stats.
#[allow(clippy::too_many_arguments)]
pub async fn run_profile(
    src: Arc<Source>,
    index: Arc<RowIndex>,
    columns: Vec<ProfileColumn>,
    nulls: NullSet,
    exec: Executor,
    ctl: JobControl,
    progress: Arc<RowsProgress>,
    opts: ProfileOptions,
) -> Result<ProfileResult, JobError> {
    while !index.is_complete() {
        tokio::select! {
            _ = ctl.cancel.cancelled() => return Err(JobError::Cancelled),
            _ = tokio::time::sleep(opts.poll) => {}
        }
    }
    let total = index.total_rows().unwrap_or(0);
    progress
        .total
        .store(total, std::sync::atomic::Ordering::Relaxed);
    let _scan = src.begin_scan();

    let window = in_flight_chunks(opts.budget, columns.len(), exec.threads());
    let capacity = topk_capacity(opts.budget, columns.len(), window);
    let cols: Arc<[ProfileColumn]> = columns.into();
    let nulls = Arc::new(nulls);
    let mut merged: Vec<ColumnStats> = cols.iter().map(|c| empty_stats(c.ty, capacity)).collect();
    let mut planner = RangePlanner::new(opts.chunk_bytes, opts.chunk_bytes);
    let mut set: JoinSet<Result<(usize, Vec<ColumnStats>), JobError>> = JoinSet::new();
    let mut reorder = ReorderBuffer::new();
    let mut next_idx = 0usize;
    let mut planned_all = false;
    loop {
        while !planned_all && set.len() + reorder.pending() < window {
            let Some(range) = planner.next_complete(&src, &index) else {
                planned_all = true;
                break;
            };
            let (src, cols, nulls, ctl, exec, progress) = (
                Arc::clone(&src),
                Arc::clone(&cols),
                Arc::clone(&nulls),
                ctl.clone(),
                exec.clone(),
                Arc::clone(&progress),
            );
            let idx = next_idx;
            next_idx += 1;
            set.spawn(async move {
                exec.run(move || {
                    profile_chunk(&src, &cols, &nulls, capacity, range, &ctl, &progress)
                        .map(|s| (idx, s))
                })
                .await
            });
        }
        let joined = tokio::select! {
            _ = ctl.cancel.cancelled() => return Err(JobError::Cancelled),
            j = set.join_next() => j,
        };
        let Some(joined) = joined else {
            break;
        };
        let (idx, stats) = match joined {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e),
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => return Err(JobError::Other(format!("profile task failed: {e}"))),
        };
        reorder.insert(idx, stats);
        while let Some(chunk) = reorder.pop_ready() {
            for (m, s) in merged.iter_mut().zip(chunk) {
                m.merge(s);
            }
        }
    }
    if ctl.cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    let columns = cols
        .iter()
        .zip(merged)
        .map(|(c, mut s)| {
            s.source = StatsSource::AllRows;
            (c.column, s)
        })
        .collect();
    Ok(ProfileResult {
        columns,
        rows: total,
        capacity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_formula() {
        let mib = 1u64 << 20;
        // One column, 16 chunks: plenty of room.
        assert_eq!(topk_capacity(256 * mib, 1, 16), 1024);
        // 13 columns × 16 chunks × 100 B × 1024 = 21 MB < 256 MiB.
        assert_eq!(topk_capacity(256 * mib, 13, 16), 1024);
        // 2,000 columns × 16 chunks: 256 MiB / 3.2 MB = 83.
        assert_eq!(topk_capacity(256 * mib, 2000, 16), 83);
        // Never below 64.
        assert_eq!(topk_capacity(mib, 2000, 16), 64);
        assert_eq!(in_flight_chunks(256 * mib, 13, 8), 16);
        // 5,000 columns: only 8 chunks fit at the minimum capacity.
        assert_eq!(in_flight_chunks(256 * mib, 5_000, 8), 8);
        assert_eq!(in_flight_chunks(mib, 50_000, 8), 1);
    }
}
