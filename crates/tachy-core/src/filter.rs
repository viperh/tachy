//! The filter job (spec §8.2, §10.1; M4-04): evaluates a compiled
//! [`Predicate`] over a parent view in parallel chunks and streams the
//! matching row ids into the new view, **in view order**, while it runs.
//!
//! # Work items
//!
//! - Parent [`ParentRows::All`]: the indexed byte range is split into chunks
//!   of about [`CHUNK_BYTES`] (the first few are smaller, so the first matches
//!   show up quickly) **aligned to index checkpoints**: each chunk spans whole
//!   groups of `stride` (1,024) rows, so it starts at a known record and row
//!   id. While the index is still building, the job follows it: it processes
//!   the published groups, waits for more ([`FilterOptions::poll`]) and ends
//!   once the index is complete, so the filter always covers the whole file.
//! - Parent [`ParentRows::Filtered`] (a refinement, `F`) and
//!   [`ParentRows::Ordered`] (a sorted view, D10): batches of
//!   [`BATCH_ROWS`] parent positions. Row ids are resolved through the index;
//!   consecutive ids share one seek. A growing parent is
//!   followed the same way as a growing index.
//!
//! Every work item runs in [`Executor::run`] and checks the job's
//! cancellation and pause tokens at least every 64 KiB of input (§4.3).
//!
//! # Publishing in order
//!
//! Chunks finish out of order, but the shared view must only ever show a
//! gap-free prefix of the result (§8.2 "results are live, in file order").
//! The orchestrator keeps a [`ReorderBuffer`]; when the next chunk in order
//! arrives it is unioned into the [`FilterRows`] bitmap (or appended to the
//! [`RowIdList`] for an ordered parent), followed by any buffered successors.
//! At most `threads × 2` chunks are in flight or buffered, which bounds
//! memory.
//!
//! # The `memmem` pre-filter
//!
//! When the predicate has a [`Predicate::required_literal`], the parent is
//! `All` and the caller says the file has no quoted newlines
//! ([`FilterOptions::prefilter`], i.e. `IndexSummary::path_used ==
//! IndexPath::Fast`), each chunk is scanned with `memmem` and only the
//! records around hits are parsed and evaluated. A record starts just after
//! the previous `\n`; its row id is the chunk's first row plus the number of
//! `\n` before it.
//!
//! That arithmetic only holds when every line of the chunk is exactly one
//! record, which blank lines and comment lines also break. So each chunk is
//! **validated**: the number of `\n` in the chunk must equal its (known) row
//! count. Otherwise the chunk's pre-filter result is discarded and the chunk
//! is parsed in full. Results are therefore identical to a full scan, with
//! one documented exception inherited from `required_literal`: a malformed
//! quoted field with bytes after its closing quote (`"ab"cd`).

use std::{
    collections::BTreeMap,
    fmt, io,
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use memchr::memmem;
use roaring::RoaringTreemap;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{
    exec::Executor,
    index::RowIndex,
    jobs::{CHECK_INTERVAL, FilterProgress, JobControl, JobError, PauseToken},
    parse::{ParseOutcome, RecordParser, RecordRanges, SkipOutcome},
    query::{EvalScratch, Predicate},
    source::Source,
    view::{FilterRows, RowIdList, RowIdWriter, View},
};

/// Nominal size of a chunk of an `All` parent (§8.2): 64 MiB.
pub const CHUNK_BYTES: u64 = 64 << 20;
/// Size of the first chunk; the next ones double up to [`CHUNK_BYTES`], so
/// the first matches appear within ~100 ms of `Enter`.
pub const FIRST_CHUNK_BYTES: u64 = 4 << 20;
/// Parent positions per work item for `Filtered` and `Ordered` parents.
pub const BATCH_ROWS: u64 = 64 * 1024;
/// How long the job sleeps while waiting for the index or the parent view to
/// grow.
pub const POLL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The view a filter runs over.
#[derive(Clone, Debug)]
pub enum ParentRows {
    /// Every row of the file, in file order.
    All,
    /// A filtered view (refinement): its row ids in file order.
    Filtered(Arc<FilterRows>),
    /// A sorted (or filtered-sorted) view: row ids in view order.
    Ordered(Arc<RowIdList>),
}

impl ParentRows {
    /// The parent of a filter applied on top of `view`.
    pub fn from_view(view: &View) -> ParentRows {
        match view {
            View::All => ParentRows::All,
            View::Filtered { rows, .. } => ParentRows::Filtered(Arc::clone(rows)),
            View::Ordered { list, .. } => ParentRows::Ordered(Arc::clone(list)),
        }
    }

    /// Published number of positions (`indexed_rows` for `All`).
    pub fn len(&self, index: &RowIndex) -> u64 {
        match self {
            ParentRows::All => index.indexed_rows(),
            ParentRows::Filtered(rows) => rows.len(),
            ParentRows::Ordered(list) => list.len(),
        }
    }

    /// No positions (yet).
    pub fn is_empty(&self, index: &RowIndex) -> bool {
        self.len(index) == 0
    }

    /// Whether positions may still be added.
    pub fn is_growing(&self, index: &RowIndex) -> bool {
        match self {
            ParentRows::All => !index.is_complete(),
            ParentRows::Filtered(rows) => rows.is_growing(),
            ParentRows::Ordered(list) => list.is_growing(),
        }
    }

    /// Row ids of positions `first .. first + count` (identity for `All`).
    pub fn row_ids(&self, first: u64, count: usize) -> Vec<u64> {
        match self {
            ParentRows::All => (first..first.saturating_add(count as u64)).collect(),
            ParentRows::Filtered(rows) => rows.row_ids(first, count),
            ParentRows::Ordered(list) => list.read(first, count),
        }
    }
}

/// Where the matching row ids go.
#[derive(Debug)]
pub enum FilterOutput {
    /// A growing bitmap (parent `All` or `Filtered`): the result is in file
    /// order.
    Bitmap(Arc<FilterRows>),
    /// A growing permutation file (parent `Ordered`): the result keeps the
    /// parent's order (D10). The UI keeps `writer.list()` for its view.
    List(RowIdWriter),
}

impl FilterOutput {
    /// The output that fits `parent` (D10): a growing [`FilterRows`] for
    /// `All` and `Filtered`, a growing [`RowIdList`] in `tmp_dir` for
    /// `Ordered`.
    pub fn for_parent(parent: &ParentRows, tmp_dir: &Path) -> io::Result<FilterOutput> {
        Ok(match parent {
            ParentRows::All | ParentRows::Filtered(_) => {
                FilterOutput::Bitmap(Arc::new(FilterRows::new_growing()))
            }
            ParentRows::Ordered(_) => FilterOutput::List(RowIdList::create_in(tmp_dir)?.1),
        })
    }

    /// The bitmap, for a [`View::Filtered`].
    pub fn rows(&self) -> Option<&Arc<FilterRows>> {
        match self {
            FilterOutput::Bitmap(rows) => Some(rows),
            FilterOutput::List(_) => None,
        }
    }

    /// The permutation file, for a [`View::Ordered`] with
    /// `OrderedKind::FilteredSorted`.
    pub fn list(&self) -> Option<&Arc<RowIdList>> {
        match self {
            FilterOutput::Bitmap(_) => None,
            FilterOutput::List(w) => Some(w.list()),
        }
    }
}

/// A hook called by each worker with its chunk index just before it returns
/// (tests use it to force out-of-order completion).
pub type ChunkHook = Arc<dyn Fn(usize) + Send + Sync>;

/// Tuning of [`run_filter`].
#[derive(Clone)]
pub struct FilterOptions {
    /// Allow the `memmem` pre-filter: pass `IndexSummary::path_used ==
    /// IndexPath::Fast` (or, while indexing, `!SniffReport::quoted_newlines`).
    /// Chunks are validated anyway (see the module docs).
    pub prefilter: bool,
    /// Nominal chunk size of an `All` parent (default [`CHUNK_BYTES`]).
    pub chunk_bytes: u64,
    /// Size of the first chunk (default [`FIRST_CHUNK_BYTES`]); the next
    /// ones double up to `chunk_bytes`.
    pub first_chunk_bytes: u64,
    /// Positions per work item of `Filtered` / `Ordered` parents (default
    /// [`BATCH_ROWS`]).
    pub batch_rows: u64,
    /// Wait between checks of a growing index or parent (default [`POLL`]).
    pub poll: Duration,
    /// Test hook, see [`ChunkHook`].
    pub chunk_hook: Option<ChunkHook>,
}

impl Default for FilterOptions {
    fn default() -> FilterOptions {
        FilterOptions {
            prefilter: false,
            chunk_bytes: CHUNK_BYTES,
            first_chunk_bytes: FIRST_CHUNK_BYTES,
            batch_rows: BATCH_ROWS,
            poll: POLL,
            chunk_hook: None,
        }
    }
}

impl fmt::Debug for FilterOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterOptions")
            .field("prefilter", &self.prefilter)
            .field("chunk_bytes", &self.chunk_bytes)
            .field("first_chunk_bytes", &self.first_chunk_bytes)
            .field("batch_rows", &self.batch_rows)
            .field("poll", &self.poll)
            .field("chunk_hook", &self.chunk_hook.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Reorder buffer
// ---------------------------------------------------------------------------

/// Releases items tagged with consecutive indices `0, 1, 2, …` in order,
/// whatever order they arrive in. Shared by the filter, export and profile
/// jobs.
#[derive(Debug)]
pub struct ReorderBuffer<T> {
    next: usize,
    pending: BTreeMap<usize, T>,
}

impl<T> Default for ReorderBuffer<T> {
    fn default() -> Self {
        ReorderBuffer {
            next: 0,
            pending: BTreeMap::new(),
        }
    }
}

impl<T> ReorderBuffer<T> {
    /// An empty buffer expecting index 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds item `idx`. Indices must be unique and `≥` the next expected one.
    pub fn insert(&mut self, idx: usize, item: T) {
        debug_assert!(idx >= self.next, "index {idx} was already released");
        self.pending.insert(idx, item);
    }

    /// The next item in order, if it has arrived.
    pub fn pop_ready(&mut self) -> Option<T> {
        let item = self.pending.remove(&self.next)?;
        self.next += 1;
        Some(item)
    }

    /// The index the buffer waits for.
    pub fn next_index(&self) -> usize {
        self.next
    }

    /// Items held back because an earlier one is missing.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

// ---------------------------------------------------------------------------
// Shared worker helpers
// ---------------------------------------------------------------------------

/// Calls [`JobControl::check`] every [`CHECK_INTERVAL`] bytes of input.
#[derive(Debug)]
pub(crate) struct Ticker<'a> {
    ctl: &'a JobControl,
    since: u64,
}

impl<'a> Ticker<'a> {
    pub(crate) fn new(ctl: &'a JobControl) -> Self {
        Ticker { ctl, since: 0 }
    }

    pub(crate) fn tick(&mut self, bytes: u64) -> Result<(), JobError> {
        self.since += bytes;
        if self.since >= CHECK_INTERVAL as u64 {
            self.since = 0;
            self.ctl.check()?;
        }
        Ok(())
    }
}

/// Resolves row ids to record offsets. When the next id is at or shortly
/// after the last row parsed, it skips forward from there; otherwise it seeks
/// through the index. Ascending ids therefore cost one seek per run.
#[derive(Debug)]
pub(crate) struct RowSeeker {
    pub(crate) parser: RecordParser,
    cursor: Option<(u64, u64)>,
}

impl RowSeeker {
    pub(crate) fn new(src: &Source) -> Self {
        RowSeeker {
            parser: RecordParser::new(src.dialect()),
            cursor: None,
        }
    }

    /// An offset to parse row `id` from (blank lines may precede it), or
    /// `None` when the row is not indexed.
    pub(crate) fn seek(&mut self, src: &Source, index: &RowIndex, id: u64) -> Option<u64> {
        let near = 2 * index.stride();
        match self.cursor {
            Some((row, off)) if id == row => Some(off),
            Some((row, off)) if id > row && id - row <= near => {
                match self.parser.skip(src.bytes(), off, id - row) {
                    SkipOutcome::Skipped { next } => Some(next),
                    SkipOutcome::Eof { .. } => None,
                }
            }
            _ => index.offset_of(id, src, &mut self.parser),
        }
    }

    /// Parses row `id` into `rec`. Returns the bytes consumed, or `None` when
    /// it is not indexed or past EOF.
    pub(crate) fn parse(
        &mut self,
        src: &Source,
        index: &RowIndex,
        id: u64,
        rec: &mut RecordRanges,
    ) -> Option<u64> {
        let Some(off) = self.seek(src, index, id) else {
            self.cursor = None;
            return None;
        };
        match self.parser.parse_at(src.bytes(), off, rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                self.cursor = Some((id + 1, next));
                Some(next - off)
            }
            ParseOutcome::Eof => {
                self.cursor = None;
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Work items
// ---------------------------------------------------------------------------

/// One unit of work.
#[derive(Clone, Copy, Debug)]
enum Work {
    /// `rows` records of an `All` parent from row `first_row` at byte
    /// `start` up to byte `end` (a checkpoint, or EOF).
    Range {
        first_row: u64,
        rows: u64,
        start: u64,
        end: u64,
    },
    /// Parent positions `first .. first + count`.
    Ids { first: u64, count: u64 },
}

enum Matches {
    Bitmap(RoaringTreemap),
    List(Vec<u64>),
}

impl Matches {
    fn len(&self) -> u64 {
        match self {
            Matches::Bitmap(b) => b.len(),
            Matches::List(v) => v.len() as u64,
        }
    }
}

struct ChunkOut {
    idx: usize,
    matches: Matches,
    bytes: u64,
    positions: u64,
}

struct Ctx {
    src: Arc<Source>,
    index: Arc<RowIndex>,
    pred: Predicate,
    parent: ParentRows,
    finder: Option<memmem::Finder<'static>>,
    hook: Option<ChunkHook>,
}

fn run_work(ctx: &Ctx, idx: usize, work: Work, ctl: &JobControl) -> Result<ChunkOut, JobError> {
    ctl.check()?;
    let out = match work {
        Work::Range {
            first_row,
            rows,
            start,
            end,
        } => {
            let pre = match &ctx.finder {
                Some(f) => prefilter_chunk(ctx, f, first_row, rows, start, end, ctl)?,
                None => None,
            };
            let matches = match pre {
                Some(b) => b,
                None => range_chunk(ctx, first_row, rows, start, ctl)?,
            };
            ChunkOut {
                idx,
                matches: Matches::Bitmap(matches),
                bytes: end - start,
                positions: rows,
            }
        }
        Work::Ids { first, count } => {
            let (matches, bytes) = ids_chunk(ctx, first, count, ctl)?;
            ChunkOut {
                idx,
                matches,
                bytes,
                positions: count,
            }
        }
    };
    if let Some(hook) = &ctx.hook {
        hook(idx);
    }
    Ok(out)
}

/// Appends `v`, which is larger than every value in `b` (falls back to an
/// insert otherwise).
fn push_sorted(b: &mut RoaringTreemap, v: u64) {
    if b.try_push(v).is_err() {
        b.insert(v);
    }
}

/// Parses `rows` records from `start` and evaluates each one.
fn range_chunk(
    ctx: &Ctx,
    first_row: u64,
    rows: u64,
    start: u64,
    ctl: &JobControl,
) -> Result<RoaringTreemap, JobError> {
    let bytes = ctx.src.bytes();
    let mut parser = RecordParser::new(ctx.src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = EvalScratch::new();
    let mut ticker = Ticker::new(ctl);
    let mut out = RoaringTreemap::new();
    let mut pos = start;
    for r in 0..rows {
        match parser.parse_at(bytes, pos, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                if ctx.pred.eval_record(bytes, &rec, &mut scratch) {
                    push_sorted(&mut out, first_row + r);
                }
                ticker.tick(next - pos)?;
                pos = next;
            }
            ParseOutcome::Eof => break,
        }
    }
    Ok(out)
}

/// The `memmem` pre-filter over one checkpoint-aligned chunk. `None` when the
/// chunk fails validation (a line that is not exactly one record) and must
/// be parsed in full.
fn prefilter_chunk(
    ctx: &Ctx,
    finder: &memmem::Finder<'_>,
    first_row: u64,
    rows: u64,
    start: u64,
    end: u64,
    ctl: &JobControl,
) -> Result<Option<RoaringTreemap>, JobError> {
    let bytes = ctx.src.bytes();
    let (start, end) = (start as usize, end as usize);
    let n = finder.needle().len();
    let mut parser = RecordParser::new(ctx.src.dialect());
    let mut rec = RecordRanges::default();
    let mut scratch = EvalScratch::new();
    let mut out = RoaringTreemap::new();
    // Newlines in `start..counted` are `newlines`.
    let mut counted = start;
    let mut newlines = 0u64;
    let mut pos = start;
    let window = CHECK_INTERVAL;
    while pos < end {
        ctl.check()?;
        let win_end = (pos + window).min(end);
        let search_end = (win_end + n - 1).min(end);
        let Some(i) = finder.find(&bytes[pos..search_end]) else {
            pos = win_end;
            continue;
        };
        let hit = pos + i;
        if hit >= win_end {
            pos = win_end;
            continue;
        }
        let rec_start = memchr::memrchr(b'\n', &bytes[start..hit]).map_or(start, |j| start + j + 1);
        newlines += memchr::memchr_iter(b'\n', &bytes[counted..rec_start]).count() as u64;
        counted = rec_start;
        if newlines >= rows {
            return Ok(None);
        }
        let next = match parser.parse_at(bytes, rec_start as u64, &mut rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
            ParseOutcome::Eof => return Ok(None),
        };
        if rec.start != rec_start as u64 {
            // A blank or comment line: not one record per line.
            return Ok(None);
        }
        if ctx.pred.eval_record(bytes, &rec, &mut scratch) {
            push_sorted(&mut out, first_row + newlines);
        }
        // Skip the rest of the record: later hits in it are the same row.
        pos = (next as usize).max(hit + 1);
    }
    newlines += memchr::memchr_iter(b'\n', &bytes[counted..end]).count() as u64;
    // Every record ends with exactly one `\n`, except a last record at EOF
    // without a trailing newline.
    let unterminated_last = end == bytes.len() && bytes.last() != Some(&b'\n');
    let expected = rows - u64::from(unterminated_last);
    Ok((newlines == expected).then_some(out))
}

/// Evaluates parent positions `first .. first + count`.
fn ids_chunk(
    ctx: &Ctx,
    first: u64,
    count: u64,
    ctl: &JobControl,
) -> Result<(Matches, u64), JobError> {
    let ids = ctx.parent.row_ids(first, count as usize);
    let src = &*ctx.src;
    let bytes = src.bytes();
    let mut seeker = RowSeeker::new(src);
    let mut rec = RecordRanges::default();
    let mut scratch = EvalScratch::new();
    let mut ticker = Ticker::new(ctl);
    let mut total = 0u64;
    let mut eval = |id: u64, seeker: &mut RowSeeker| -> Result<bool, JobError> {
        let Some(n) = seeker.parse(src, &ctx.index, id, &mut rec) else {
            return Ok(false);
        };
        total += n;
        ticker.tick(n)?;
        Ok(ctx.pred.eval_record(bytes, &rec, &mut scratch))
    };
    let matches = match ctx.parent {
        ParentRows::Ordered(_) => {
            // Walk in row-id order (forward seeks), emit in parent order.
            let mut order: Vec<(u64, usize)> =
                ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
            order.sort_unstable();
            let mut hit = vec![false; ids.len()];
            for (id, i) in order {
                hit[i] = eval(id, &mut seeker)?;
            }
            Matches::List(
                ids.iter()
                    .zip(hit)
                    .filter_map(|(&id, h)| h.then_some(id))
                    .collect(),
            )
        }
        _ => {
            let mut out = RoaringTreemap::new();
            for &id in &ids {
                if eval(id, &mut seeker)? {
                    out.insert(id);
                }
            }
            Matches::Bitmap(out)
        }
    };
    Ok((matches, total))
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

enum Step {
    Work(Work),
    Wait,
    Done,
}

/// A checkpoint-aligned chunk: `rows` records from row `first_row`, bytes
/// `start .. end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowRange {
    pub(crate) first_row: u64,
    pub(crate) rows: u64,
    pub(crate) start: u64,
    pub(crate) end: u64,
}

/// Splits an `All` parent into checkpoint-aligned chunks, following a
/// growing index.
#[derive(Debug)]
pub(crate) struct RangePlanner {
    next_k: u64,
    chunk: u64,
    max_chunk: u64,
    done: bool,
}

impl RangePlanner {
    pub(crate) fn new(first_chunk: u64, max_chunk: u64) -> Self {
        RangePlanner {
            next_k: 0,
            chunk: first_chunk.clamp(1, max_chunk.max(1)),
            max_chunk: max_chunk.max(1),
            done: false,
        }
    }

    /// The next chunk of a **complete** index, or `None` at the end.
    pub(crate) fn next_complete(&mut self, src: &Source, index: &RowIndex) -> Option<RowRange> {
        debug_assert!(index.is_complete());
        match self.next(src, index, true) {
            Step::Work(Work::Range {
                first_row,
                rows,
                start,
                end,
            }) => Some(RowRange {
                first_row,
                rows,
                start,
                end,
            }),
            _ => None,
        }
    }

    /// The next chunk `(first_row, rows, start, end)`. `idle`: nothing is in
    /// flight, so a short chunk of what is indexed so far is better than
    /// waiting.
    fn next(&mut self, src: &Source, index: &RowIndex, idle: bool) -> Step {
        if self.done {
            return Step::Done;
        }
        let stride = index.stride();
        // Read `complete` first: once it is true, the counts are final.
        let complete = index.is_complete();
        let published = index.published_checkpoints();
        let total = index.indexed_rows();
        let first_row = self.next_k * stride;
        let Some(start) = index.checkpoint(self.next_k) else {
            if complete {
                self.done = true;
                return Step::Done;
            }
            return Step::Wait;
        };
        if complete && first_row >= total {
            self.done = true;
            return Step::Done;
        }
        // Whole groups: checkpoints `next_k + 1 ..= last` are their ends.
        let last = published.saturating_sub(1);
        let target = start.saturating_add(self.chunk);
        let mut k1 = None;
        if last > self.next_k {
            // Binary search for the first checkpoint ≥ target.
            let (mut lo, mut hi) = (self.next_k + 1, last);
            if index.checkpoint(hi).is_some_and(|o| o >= target) {
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if index.checkpoint(mid).is_some_and(|o| o >= target) {
                        hi = mid;
                    } else {
                        lo = mid + 1;
                    }
                }
                k1 = Some(lo);
            } else if !complete && idle {
                k1 = Some(last);
            }
        }
        let work = match k1 {
            Some(k1) => {
                let end = index.checkpoint(k1).expect("published checkpoint");
                self.next_k = k1;
                Work::Range {
                    first_row,
                    rows: (k1 * stride - first_row),
                    start,
                    end,
                }
            }
            None if complete => {
                self.done = true;
                Work::Range {
                    first_row,
                    rows: total - first_row,
                    start,
                    end: src.len(),
                }
            }
            None => return Step::Wait,
        };
        self.chunk = (self.chunk * 2).min(self.max_chunk);
        Step::Work(work)
    }
}

/// Splits a `Filtered` / `Ordered` parent into position batches, following a
/// growing parent.
struct IdsPlanner {
    next: u64,
    batch: u64,
}

impl IdsPlanner {
    fn next(&mut self, parent: &ParentRows, index: &RowIndex, idle: bool) -> Step {
        // `growing` first: once it is false the length is final.
        let growing = parent.is_growing(index);
        let len = parent.len(index);
        let avail = len.saturating_sub(self.next);
        if avail == 0 {
            return if growing { Step::Wait } else { Step::Done };
        }
        if avail < self.batch && growing && !idle {
            return Step::Wait;
        }
        let count = avail.min(self.batch);
        let first = self.next;
        self.next += count;
        Step::Work(Work::Ids { first, count })
    }
}

enum Planner {
    Range(RangePlanner),
    Ids(IdsPlanner),
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

enum Sink {
    Bitmap(Arc<FilterRows>),
    List(Option<RowIdWriter>),
}

impl Sink {
    async fn publish(&mut self, m: Matches) -> Result<(), JobError> {
        match (self, m) {
            (Sink::Bitmap(rows), Matches::Bitmap(b)) => {
                if !b.is_empty() {
                    rows.union_with(&b);
                }
                Ok(())
            }
            (Sink::Bitmap(rows), Matches::List(v)) => {
                rows.insert_many(v);
                Ok(())
            }
            (Sink::List(slot), m) => {
                let ids: Vec<u64> = match m {
                    Matches::List(v) => v,
                    Matches::Bitmap(b) => b.iter().collect(),
                };
                let mut w = slot.take().expect("the writer is always put back");
                let w = tokio::task::spawn_blocking(move || -> io::Result<RowIdWriter> {
                    w.extend(&ids)?;
                    w.publish()?;
                    Ok(w)
                })
                .await
                .map_err(|e| JobError::Other(format!("filter output task failed: {e}")))?
                .map_err(|e| JobError::io("writing the filter result", e))?;
                *slot = Some(w);
                Ok(())
            }
        }
    }

    async fn finish(self) -> Result<(), JobError> {
        match self {
            Sink::Bitmap(rows) => {
                rows.finish();
                Ok(())
            }
            Sink::List(slot) => {
                let w = slot.expect("the writer is always put back");
                tokio::task::spawn_blocking(move || w.finish())
                    .await
                    .map_err(|e| JobError::Other(format!("filter output task failed: {e}")))?
                    .map_err(|e| JobError::io("writing the filter result", e))?;
                Ok(())
            }
        }
    }
}

/// Runs a filter job (M4-04) with default [`FilterOptions`] except
/// `prefilter`. See [`run_filter_with`].
#[allow(clippy::too_many_arguments)]
pub async fn run_filter(
    parent: ParentRows,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    pred: Predicate,
    out: FilterOutput,
    exec: Executor,
    cancel: CancellationToken,
    pause: PauseToken,
    progress: Arc<FilterProgress>,
    prefilter: bool,
) -> Result<(), JobError> {
    let opts = FilterOptions {
        prefilter,
        ..FilterOptions::default()
    };
    run_filter_with(
        parent, src, index, pred, out, exec, cancel, pause, progress, opts,
    )
    .await
}

/// Runs a filter job (M4-04): see the module docs.
///
/// - Matches are published into `out` as a gap-free prefix in view order;
///   on success the output is marked complete (`growing = false`).
/// - `progress`: bytes scanned / to scan (the total grows with the index
///   for an `All` parent; it is an estimate from the average record size for
///   other parents) and matches published.
/// - Cancellation returns [`JobError::Cancelled`] promptly and leaves `out`
///   growing; the caller discards it (pops the view).
#[allow(clippy::too_many_arguments)]
pub async fn run_filter_with(
    parent: ParentRows,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    pred: Predicate,
    out: FilterOutput,
    exec: Executor,
    cancel: CancellationToken,
    pause: PauseToken,
    progress: Arc<FilterProgress>,
    opts: FilterOptions,
) -> Result<(), JobError> {
    let ctl = JobControl { cancel, pause };
    let _scan = src.begin_scan();
    let finder = match (&parent, pred.required_literal()) {
        (ParentRows::All, Some(lit)) if opts.prefilter && !lit.contains(&b'\n') => {
            Some(memmem::Finder::new(lit).into_owned())
        }
        _ => None,
    };
    let mut planner = match parent {
        ParentRows::All => {
            Planner::Range(RangePlanner::new(opts.first_chunk_bytes, opts.chunk_bytes))
        }
        _ => Planner::Ids(IdsPlanner {
            next: 0,
            batch: opts.batch_rows.max(1),
        }),
    };
    let data_start = src.data_start();
    let ctx = Arc::new(Ctx {
        src,
        index,
        pred,
        parent,
        finder,
        hook: opts.chunk_hook.clone(),
    });
    let mut sink = match out {
        FilterOutput::Bitmap(rows) => Sink::Bitmap(rows),
        FilterOutput::List(w) => Sink::List(Some(w)),
    };
    let window = exec.threads().max(1) * 2;
    let mut set: JoinSet<Result<ChunkOut, JobError>> = JoinSet::new();
    let mut reorder: ReorderBuffer<ChunkOut> = ReorderBuffer::new();
    let mut next_idx = 0usize;
    let (mut done_bytes, mut done_positions) = (0u64, 0u64);

    loop {
        if ctl.cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        let mut finished = false;
        while set.len() + reorder.pending() < window {
            let idle = set.is_empty() && reorder.pending() == 0;
            let step = match &mut planner {
                Planner::Range(p) => {
                    let s = p.next(&ctx.src, &ctx.index, idle);
                    // The byte total grows with the index.
                    let covered = if ctx.index.is_complete() {
                        ctx.src.len()
                    } else {
                        let last = ctx.index.published_checkpoints().saturating_sub(1);
                        ctx.index.checkpoint(last).unwrap_or(data_start)
                    };
                    progress
                        .bytes_total
                        .fetch_max(covered.saturating_sub(data_start), Ordering::Relaxed);
                    s
                }
                Planner::Ids(p) => p.next(&ctx.parent, &ctx.index, idle),
            };
            match step {
                Step::Work(work) => {
                    let (ctx, ctl, exec) = (Arc::clone(&ctx), ctl.clone(), exec.clone());
                    let idx = next_idx;
                    next_idx += 1;
                    set.spawn(
                        async move { exec.run(move || run_work(&ctx, idx, work, &ctl)).await },
                    );
                }
                Step::Wait => break,
                Step::Done => {
                    finished = true;
                    break;
                }
            }
        }
        if set.is_empty() {
            if finished {
                break;
            }
            tokio::select! {
                _ = ctl.cancel.cancelled() => return Err(JobError::Cancelled),
                _ = tokio::time::sleep(opts.poll) => {}
            }
            continue;
        }
        let joined = tokio::select! {
            _ = ctl.cancel.cancelled() => return Err(JobError::Cancelled),
            j = set.join_next() => j.expect("the set is not empty"),
        };
        let chunk = match joined {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(e)) => return Err(e),
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => return Err(JobError::Other(format!("filter task failed: {e}"))),
        };
        progress
            .bytes_done
            .fetch_add(chunk.bytes, Ordering::Relaxed);
        done_bytes += chunk.bytes;
        done_positions += chunk.positions;
        if let Planner::Ids(_) = planner
            && done_positions > 0
        {
            let len = ctx.parent.len(&ctx.index);
            let estimate = (done_bytes as f64 / done_positions as f64 * len as f64) as u64;
            progress
                .bytes_total
                .store(estimate.max(done_bytes), Ordering::Relaxed);
        }
        reorder.insert(chunk.idx, chunk);
        while let Some(ready) = reorder.pop_ready() {
            let n = ready.matches.len();
            sink.publish(ready.matches).await?;
            progress.matches.fetch_add(n, Ordering::Relaxed);
        }
    }
    if ctl.cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    sink.finish().await?;
    progress
        .bytes_total
        .store(progress.bytes_done(), Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_releases_in_order() {
        let mut r = ReorderBuffer::new();
        let mut out = Vec::new();
        for idx in [3usize, 1, 4, 0, 2, 6, 5] {
            r.insert(idx, idx * 10);
            while let Some(v) = r.pop_ready() {
                out.push(v);
            }
        }
        assert_eq!(out, vec![0, 10, 20, 30, 40, 50, 60]);
        assert_eq!(r.pending(), 0);
        assert_eq!(r.next_index(), 7);
    }

    #[test]
    fn reorder_holds_back_after_a_gap() {
        let mut r = ReorderBuffer::new();
        r.insert(1, 'b');
        r.insert(2, 'c');
        assert_eq!(r.pop_ready(), None);
        assert_eq!(r.pending(), 2);
        r.insert(0, 'a');
        assert_eq!(r.pop_ready(), Some('a'));
        assert_eq!(r.pop_ready(), Some('b'));
        assert_eq!(r.pop_ready(), Some('c'));
        assert_eq!(r.pop_ready(), None);
    }
}
