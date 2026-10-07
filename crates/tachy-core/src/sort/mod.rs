//! External merge sort producing a permutation file of row ids (spec §8.4,
//! M5-03).
//!
//! - [`SortSpec::parse`]: `price:desc, ts` → [`SortKey`]s.
//! - [`estimate()`]: disk / RAM estimate for the palette preview.
//! - [`run_sort`]: the job. It reads the parent view's rows, encodes the
//!   first key of each row into a 24-byte record ([`key`]), buffers records
//!   up to the RAM cap, sorts buffer slices in parallel on the [`Executor`]
//!   and writes each sorted slice as a run ([`runs`]), then k-way merges the
//!   runs ([`merge`]) into a [`RowIdList`], in one or more passes. When
//!   everything fits in one buffer, sorted slices are merged straight into
//!   the permutation file without writing runs.
//!
//! Order: keys in priority order, each typed by the column's effective type;
//! nulls and unparseable values last in both directions; ties broken by
//! **row id ascending** (stable with respect to file order, also for
//! descending sorts and non-`All` parents).
//!
//! Files: runs live in a `tachy-job-*` [`tempfile::TempDir`] in the temp dir,
//! removed when the job ends (done, failed, cancelled or panicked). The
//! permutation file is a `tachy-perm-*` `NamedTempFile` directly in the temp
//! dir (not in the job dir, which is deleted when the job ends), owned by
//! the [`RowIdList`] and deleted with its view.

pub mod estimate;
pub mod key;
pub mod merge;
pub mod runs;

use std::{cmp::Ordering, fmt, ops::Range, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

pub use estimate::{SortEstimate, check_space, estimate, format_bytes, free_space};
pub use key::{KeySpec, SortRecord};

use crate::{
    column::ColumnMeta,
    dialect::Encoding,
    exec::Executor,
    index::RowIndex,
    jobs::{JobControl, JobError, SortPhase, SortProgress},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    query::{
        self, ColumnRef, Expr, Operand, QueryError, ResolvedExpr, ResolvedOperand,
        lexer::{TokenKind, lex},
    },
    source::Source,
    types::{ColType, NullSet},
    view::{RowIdList, View},
};

/// One sort key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SortKey {
    /// Index into the tab's columns.
    pub column: usize,
    /// Descending order.
    pub descending: bool,
    /// Case-folded string comparison (`str` / `enum` only).
    pub ci: bool,
}

impl SortKey {
    /// An ascending, case-sensitive key on `column`.
    pub fn asc(column: usize) -> SortKey {
        SortKey {
            column,
            descending: false,
            ci: false,
        }
    }

    /// A descending, case-sensitive key on `column`.
    pub fn desc(column: usize) -> SortKey {
        SortKey {
            column,
            descending: true,
            ci: false,
        }
    }
}

/// The header indicator of `column` in a view sorted by `keys` (§11.3): `▲`
/// or `▼`, with the 1-based priority appended for multi-key sorts (`▲1`,
/// `▼2`). `None` when the column is not a key.
pub fn header_indicator(keys: &[SortKey], column: usize) -> Option<String> {
    let (i, k) = keys.iter().enumerate().find(|(_, k)| k.column == column)?;
    let arrow = if k.descending { '▼' } else { '▲' };
    Some(if keys.len() > 1 {
        format!("{arrow}{}", i + 1)
    } else {
        arrow.to_string()
    })
}

/// A parsed `sort` command: keys in priority order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortSpec {
    /// The keys, highest priority first. Never empty.
    pub keys: Vec<SortKey>,
}

impl SortSpec {
    /// Parses `key ("," key)*`, `key := column (":" dir)? (":" "ci")?`,
    /// `dir := asc | desc` (default `asc`). Columns follow the query
    /// language (identifier, `` `quoted name` `` or `$N`, M4-01). `ci` is
    /// only valid on `str` / `enum` columns. Errors carry a byte span into
    /// `input`.
    pub fn parse(input: &str, columns: &[ColumnMeta]) -> Result<SortSpec, QueryError> {
        let names: Vec<_> = columns.iter().map(|c| c.name.clone()).collect();
        let mut keys = Vec::new();
        for key in split_outside_backticks(input, ',', 0..input.len()) {
            let parts = split_outside_backticks(input, ':', key.clone());
            let col = trim(input, parts[0].clone());
            if col.is_empty() {
                let at = if key.is_empty() {
                    key.start..key.start
                } else {
                    key.clone()
                };
                return Err(QueryError::new("expected a column", at));
            }
            let column = resolve_column(input, col, &names)?;
            let mut k = SortKey::asc(column);
            let (mut dir_seen, mut ci_seen) = (false, false);
            for part in &parts[1..] {
                let span = trim(input, part.clone());
                match &input[span.clone()] {
                    "asc" | "desc" if !dir_seen && !ci_seen => {
                        dir_seen = true;
                        k.descending = &input[span] == "desc";
                    }
                    "ci" if !ci_seen => {
                        ci_seen = true;
                        let c = &columns[column];
                        if !matches!(c.ty(), ColType::Str | ColType::Enum) {
                            return Err(QueryError::new(
                                format!(
                                    "ci only applies to str and enum columns; {} is {}",
                                    c.name.query,
                                    c.ty()
                                ),
                                span,
                            ));
                        }
                        k.ci = true;
                    }
                    "" => return Err(QueryError::new("expected asc, desc or ci after :", span)),
                    other => {
                        return Err(QueryError::new(
                            format!("expected asc, desc or ci, found \"{other}\""),
                            span,
                        ));
                    }
                }
            }
            keys.push(k);
        }
        Ok(SortSpec { keys })
    }

    /// The canonical text: `price:desc, ts, name:asc:ci` (for job titles and
    /// saved views).
    pub fn display(&self, columns: &[ColumnMeta]) -> String {
        DisplaySpec(self, columns).to_string()
    }
}

struct DisplaySpec<'a>(&'a SortSpec, &'a [ColumnMeta]);

impl fmt::Display for DisplaySpec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, k) in self.0.keys.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            match self.1.get(k.column) {
                Some(c) => ColumnRef::Name {
                    name: c.name.query.clone(),
                    span: 0..0,
                }
                .fmt(f)?,
                None => write!(f, "${}", k.column + 1)?,
            }
            match (k.descending, k.ci) {
                (true, false) => f.write_str(":desc")?,
                (true, true) => f.write_str(":desc:ci")?,
                (false, true) => f.write_str(":asc:ci")?,
                (false, false) => {}
            }
        }
        Ok(())
    }
}

/// `range` trimmed of ASCII whitespace.
fn trim(input: &str, range: Range<usize>) -> Range<usize> {
    let s = &input[range.clone()];
    let start = range.start + (s.len() - s.trim_start().len());
    let end = range.end - (s.len() - s.trim_end().len());
    start..end.max(start)
}

/// Splits `input[range]` on `sep` outside backticks.
fn split_outside_backticks(input: &str, sep: char, range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = range.start;
    let mut in_tick = false;
    for (i, c) in input[range.clone()].char_indices() {
        let i = range.start + i;
        if c == '`' {
            in_tick = !in_tick;
        } else if c == sep && !in_tick {
            out.push(start..i);
            start = i + 1;
        }
    }
    out.push(start..range.end);
    out
}

/// Resolves one column reference with the query lexer and resolver.
fn resolve_column(
    input: &str,
    span: Range<usize>,
    names: &[crate::column::ColumnName],
) -> Result<usize, QueryError> {
    let shift = |e: QueryError| {
        QueryError::new(
            e.message,
            e.span.start + span.start..e.span.end + span.start,
        )
    };
    let tokens = lex(&input[span.clone()]).map_err(shift)?;
    let abs = |s: &Range<usize>| s.start + span.start..s.end + span.start;
    let cref = match tokens.as_slice() {
        [t] => match &t.kind {
            TokenKind::Ident(name) | TokenKind::QuotedIdent(name) => ColumnRef::Name {
                name: name.clone(),
                span: abs(&t.span),
            },
            TokenKind::ColIndex(n) => ColumnRef::Index {
                n: *n,
                span: abs(&t.span),
            },
            TokenKind::Keyword(k) => {
                return Err(QueryError::new(
                    format!(
                        "'{0}' is a keyword; write `{0}` to use it as a column name",
                        k.as_str()
                    ),
                    abs(&t.span),
                ));
            }
            _ => return Err(QueryError::new("expected a column", abs(&t.span))),
        },
        _ => return Err(QueryError::new("expected one column name", span)),
    };
    match query::resolve(Expr::Truthy(Operand::Column(cref)), names)? {
        ResolvedExpr::Truthy(ResolvedOperand::Column { index, .. }) => Ok(index),
        _ => unreachable!("a column reference resolves to a column"),
    }
}

// ---------------------------------------------------------------------------
// Comparator
// ---------------------------------------------------------------------------

/// What every sort worker shares.
#[derive(Debug)]
pub(crate) struct SortCtx {
    pub(crate) src: Arc<Source>,
    pub(crate) index: Arc<RowIndex>,
    pub(crate) keys: Vec<KeySpec>,
    pub(crate) nulls: NullSet,
    pub(crate) enc: Encoding,
}

/// The record comparator: prefix, then full values read from the mmap when
/// needed (truncated strings, later keys), then row id. One per worker.
pub(crate) struct TieBreaker {
    ctx: Arc<SortCtx>,
    parser: RecordParser,
    cache: [(Option<u64>, RecordRanges); 2],
    scratch: [Vec<u8>; 2],
    text: [String; 2],
}

impl TieBreaker {
    pub(crate) fn new(ctx: Arc<SortCtx>) -> TieBreaker {
        TieBreaker {
            parser: RecordParser::new(ctx.src.dialect()),
            ctx,
            cache: Default::default(),
            scratch: Default::default(),
            text: Default::default(),
        }
    }

    /// Total order on records.
    pub(crate) fn cmp(&mut self, a: &SortRecord, b: &SortRecord) -> Ordering {
        match a.prefix.cmp(&b.prefix) {
            Ordering::Equal => {}
            o => return o,
        }
        if a.row_id == b.row_id {
            return Ordering::Equal;
        }
        let start = usize::from(!self.ctx.keys[0].is_truncated(&a.prefix));
        if start < self.ctx.keys.len() {
            let o = self.full(a.row_id, b.row_id, start);
            if o != Ordering::Equal {
                return o;
            }
        }
        a.row_id.cmp(&b.row_id)
    }

    /// Loads `row` into a cache slot other than `avoid`; returns the slot.
    fn load(&mut self, row: u64, avoid: Option<usize>) -> usize {
        if let Some(i) = self.cache.iter().position(|(r, _)| *r == Some(row)) {
            return i;
        }
        let slot = match avoid {
            Some(0) => 1,
            _ => 0,
        };
        let bytes = self.ctx.src.bytes();
        let (r, rec) = &mut self.cache[slot];
        match self
            .ctx
            .index
            .offset_of(row, &self.ctx.src, &mut self.parser)
        {
            Some(off) => {
                if matches!(self.parser.parse_at(bytes, off, rec), ParseOutcome::Eof) {
                    rec.fields.clear();
                }
            }
            None => rec.fields.clear(),
        }
        *r = Some(row);
        slot
    }

    /// Compares rows `a` and `b` on keys `start..` with full values.
    fn full(&mut self, a: u64, b: u64, start: usize) -> Ordering {
        let sa = self.load(a, None);
        let sb = self.load(b, Some(sa));
        let ctx = Arc::clone(&self.ctx);
        let bytes = ctx.src.bytes();
        let Self {
            parser,
            cache,
            scratch,
            text,
            ..
        } = self;
        let [sc_a, sc_b] = scratch;
        let [tx_a, tx_b] = text;
        for spec in &ctx.keys[start..] {
            let ra = &cache[sa].1;
            let rb = &cache[sb].1;
            let va = (spec.field < ra.fields.len())
                .then(|| parser.field_value(bytes, ra, spec.field, sc_a));
            let vb = (spec.field < rb.fields.len())
                .then(|| parser.field_value(bytes, rb, spec.field, sc_b));
            let o = key::compare_values(spec, va, vb, &ctx.nulls, ctx.enc, tx_a, tx_b);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

/// Tuning knobs, mostly for tests.
#[derive(Clone, Debug)]
pub struct SortOptions {
    /// Merge fan-in; default `min(256, ram_cap / 1 MiB)` (at least 2). Tests
    /// set it low to force several merge passes.
    pub fan_in: Option<usize>,
    /// Largest in-memory slice sorted by one worker (default 1M records,
    /// ~24 MB, well under a second; cancellation is checked between slices).
    pub max_slice_records: usize,
    /// Parent positions per extraction chunk (default 64 Ki).
    pub chunk_rows: u64,
}

impl Default for SortOptions {
    fn default() -> SortOptions {
        SortOptions {
            fan_in: None,
            max_slice_records: 1 << 20,
            chunk_rows: 64 * 1024,
        }
    }
}

/// Everything a sort needs.
#[derive(Clone, Debug)]
pub struct SortJob {
    /// The file.
    pub src: Arc<Source>,
    /// Its row index (complete before extraction starts).
    pub index: Arc<RowIndex>,
    /// The view being sorted: `All`, `Filtered` or `Ordered`.
    pub parent: View,
    /// Keys in priority order (non-empty).
    pub keys: Vec<SortKey>,
    /// The tab's columns (effective types, `source_index`).
    pub columns: Vec<ColumnMeta>,
    /// Null spellings.
    pub nulls: NullSet,
    /// Memory for the record buffer, from the job budget (§10.4).
    pub ram_cap: u64,
    /// `--tmp`.
    pub tmp_dir: PathBuf,
    /// Tuning.
    pub options: SortOptions,
}

/// Runs a sort job (M5-03). Returns the permutation file for an
/// `Ordered { Sorted }` view.
///
/// - Waits (polling every 100 ms) while the parent is still growing: a
///   running filter, or the index while it builds. Sorting a partial set is
///   never right.
/// - Checks the free space of `tmp_dir` first (Unix).
/// - `ctl` is checked every 64 KiB of input during extraction, between
///   slices while sorting, and every 1 MiB of merge output; pausing parks the
///   workers there.
/// - Any I/O error (disk full) → [`JobError::Io`]; the job's files are
///   removed, the parent view is unaffected.
pub async fn run_sort(
    job: SortJob,
    exec: &Executor,
    ctl: &JobControl,
    progress: Arc<SortProgress>,
) -> Result<Arc<RowIdList>, JobError> {
    if job.keys.is_empty() {
        return Err(JobError::Other("no sort keys".into()));
    }
    let keys = job
        .keys
        .iter()
        .map(|k| {
            let c = job.columns.get(k.column).ok_or_else(|| {
                JobError::Other(format!("sort key column {} does not exist", k.column))
            })?;
            Ok(KeySpec {
                field: c.source_index,
                ty: c.ty(),
                descending: k.descending,
                ci: k.ci,
            })
        })
        .collect::<Result<Vec<_>, JobError>>()?;

    // Wait for the parent to be complete.
    while job.parent.is_growing(&job.index) {
        if ctl.cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rows = job.parent.len(&job.index);
    progress.start_phase(SortPhase::Extract, rows);

    let record_bytes = key::RECORD_BYTES as u64;
    let in_memory_guess = rows.saturating_mul(record_bytes) <= job.ram_cap;
    let disk = if in_memory_guess {
        rows * estimate::RESULT_BYTES
    } else {
        rows.saturating_mul(record_bytes + estimate::RESULT_BYTES)
    };
    check_space(&job.tmp_dir, disk)?;

    let tmp = tempfile::Builder::new()
        .prefix("tachy-job-")
        .tempdir_in(&job.tmp_dir)
        .map_err(|e| {
            JobError::io(
                format!("creating a temp dir in {}", job.tmp_dir.display()),
                e,
            )
        })?;
    let ctx = Arc::new(SortCtx {
        enc: job.src.dialect().encoding,
        src: Arc::clone(&job.src),
        index: Arc::clone(&job.index),
        keys,
        nulls: job.nulls.clone(),
    });
    let _scan = job.src.begin_scan();

    // Advisory budget (§10.4): no minimum, so a tiny budget really spills.
    let cap_records = ((job.ram_cap / record_bytes) as usize).max(1);
    let threads = exec.threads().max(1);
    let slab_cap = cap_records
        .div_ceil(threads)
        .clamp(1, job.options.max_slice_records.max(1));
    let mut buffer = runs::Slabs::new(slab_cap);
    let mut spilled: Vec<runs::Run> = Vec::new();
    let mut extracted = 0u64;

    // Extraction pipeline: a bounded window of chunk tasks.
    // Chunks in flight stay within the buffer budget.
    let window = threads * 2;
    let chunk = job
        .options
        .chunk_rows
        .min((cap_records / window) as u64)
        .max(1);
    let mut next = 0u64;
    let mut set: JoinSet<Result<Vec<SortRecord>, JobError>> = JoinSet::new();
    let mut failure: Option<JobError> = None;
    loop {
        while failure.is_none() && set.len() < window && next < rows {
            let count = chunk.min(rows - next);
            let (ctx, ctl, exec, parent) = (
                Arc::clone(&ctx),
                ctl.clone(),
                exec.clone(),
                job.parent.clone(),
            );
            let first = next;
            set.spawn(async move {
                exec.run(move || runs::extract_chunk(&ctx, &parent, first, count, &ctl))
                    .await
            });
            next += count;
        }
        let Some(done) = set.join_next().await else {
            break;
        };
        let recs = match done {
            Ok(Ok(recs)) => recs,
            Ok(Err(e)) => {
                failure.get_or_insert(e);
                continue;
            }
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => {
                failure.get_or_insert(JobError::Cancelled);
                continue;
            }
        };
        if failure.is_some() {
            continue;
        }
        extracted += recs.len() as u64;
        progress.add(recs.len() as u64);
        buffer.extend(&recs);
        if buffer.len() >= cap_records {
            match runs::spill(
                &ctx,
                exec,
                ctl,
                &progress,
                buffer.take(),
                tmp.path(),
                spilled.len(),
            )
            .await
            {
                Ok(new) => spilled.extend(new),
                Err(e) => failure = Some(e),
            }
            progress.start_phase(SortPhase::Extract, rows);
            progress
                .done
                .store(extracted, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }

    let (_, mut writer) = RowIdList::create_in(&job.tmp_dir).map_err(|e| {
        JobError::io(
            format!("creating the permutation file in {}", job.tmp_dir.display()),
            e,
        )
    })?;
    let sink_ctx = format!("writing {}", writer.list().path().display());

    if spilled.is_empty() {
        // In memory: sort the slabs in parallel, merge them into the result.
        let slabs = buffer.take();
        progress.start_phase(SortPhase::SortRuns, slabs.len() as u64);
        let sorted = runs::sort_slabs(&ctx, exec, ctl, &progress, slabs).await?;
        progress.start_merge_pass(1, 1, extracted);
        let sources = sorted.into_iter().map(merge::RunReader::memory).collect();
        let ctx2 = Arc::clone(&ctx);
        let (ctl2, progress2) = (ctl.clone(), Arc::clone(&progress));
        writer = exec
            .run(move || {
                merge::merge_into_ids(&ctx2, sources, &mut writer, &ctl2, &progress2, &sink_ctx)
                    .map(|_| writer)
            })
            .await?;
    } else {
        if !buffer.is_empty() {
            let more = runs::spill(
                &ctx,
                exec,
                ctl,
                &progress,
                buffer.take(),
                tmp.path(),
                spilled.len(),
            )
            .await?;
            spilled.extend(more);
        }
        let mem_fan_in = (job.ram_cap / merge::READ_BUFFER as u64) as usize;
        let fan_in = job.options.fan_in.unwrap_or(mem_fan_in.min(256)).max(2);
        let read_buf = ((job.ram_cap as usize) / fan_in).clamp(64 << 10, merge::READ_BUFFER);
        writer = merge::merge_runs(
            &ctx,
            exec,
            ctl,
            &progress,
            spilled,
            fan_in,
            read_buf,
            tmp.path(),
            writer,
            extracted,
            sink_ctx,
        )
        .await?;
    }
    let list = writer
        .finish()
        .map_err(|e| JobError::io("finishing the permutation file", e))?;
    drop(tmp);
    Ok(list)
}
