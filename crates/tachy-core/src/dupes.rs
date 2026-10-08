//! Duplicate rows: the `dupes` job, which shows every row whose key occurs
//! more than once, and the `dedupe` job, which keeps the first row of each
//! key (in view order) and drops the others.
//!
//! The key is a list of columns (all columns when empty), compared on their
//! values **after column edits** (`crate::edit`), so `edit name: trim` makes
//! `"Bob "` and `"Bob"` duplicates. A missing cell (short row) equals an
//! empty one. Nothing is written to the source: the result is a new view,
//! saved with `export`.
//!
//! # Algorithm
//!
//! 1. **Extract.** The parent view (`All`, `Filtered` or `Ordered`) is read
//!    in parallel chunks of positions. Each row's key is hashed to 128 bits
//!    (two SipHash-1-3 streams). At 128 bits a collision between two
//!    different keys is not a practical concern (≈ 10⁻²² for 10⁹ rows).
//!    Each `(hash, position)` record (24 bytes) goes to one of `P`
//!    partitions by the top bits of its hash: kept in memory when `P = 1`,
//!    appended to partition files in a `tachy-job-*` temp dir otherwise. `P`
//!    is chosen so that one partition fits in the job's memory budget
//!    (§10.4).
//! 2. **Group.** Each partition is sorted by `(hash, position)`; for every
//!    run of equal hashes the selected positions go into a bitmap: the
//!    whole run when it has 2+ rows (`dupes`), or its first position
//!    (`dedupe`).
//! 3. **Output.** The selected positions are mapped back to row ids in view
//!    order: a [`FilterRows`] bitmap for `All` and `Filtered` parents, a
//!    [`RowIdList`] in the parent's order for an `Ordered` one.
//!
//! The job waits while its parent is still growing, like a sort. The temp
//! dir is removed when the job ends (done, failed, cancelled or panicked).

use std::{
    fs::File,
    hash::{DefaultHasher, Hasher},
    io::{BufReader, BufWriter, Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use roaring::RoaringTreemap;
use tokio::task::JoinSet;

use crate::{
    column::ColumnMeta,
    exec::Executor,
    filter::RowSeeker,
    index::RowIndex,
    jobs::{CHECK_INTERVAL, JobControl, JobError, RowsProgress},
    parse::{RecordParser, RecordRanges},
    query::ColumnRef,
    query::QueryError,
    sort::{check_space, resolve_column, split_outside_backticks, trim},
    source::Source,
    view::{FilterRows, RowIdList, View},
};

/// What the job selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DupeMode {
    /// Every row of every key that occurs 2+ times (`dupes`).
    Show,
    /// The first row of every key, in view order (`dedupe`).
    Remove,
}

impl DupeMode {
    /// The command name: `dupes` or `dedupe`.
    pub fn command(self) -> &'static str {
        match self {
            DupeMode::Show => "dupes",
            DupeMode::Remove => "dedupe",
        }
    }
}

/// A parsed `dupes` / `dedupe` command.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DupeSpec {
    /// Key columns (indices into the tab's columns), in the order given.
    /// Empty = every column.
    pub columns: Vec<usize>,
    /// Show or remove.
    pub mode: DupeMode,
}

impl DupeSpec {
    /// Parses `column ("," column)*`, or nothing for every column. Columns
    /// follow the query language (identifier, `` `quoted name` `` or `$N`),
    /// like sort keys. Errors carry a byte span into `input`.
    pub fn parse(
        input: &str,
        mode: DupeMode,
        columns: &[ColumnMeta],
    ) -> Result<DupeSpec, QueryError> {
        let names: Vec<_> = columns.iter().map(|c| c.name.clone()).collect();
        let mut keys = Vec::new();
        if !input.trim().is_empty() {
            for part in split_outside_backticks(input, ',', 0..input.len()) {
                let col = trim(input, part.clone());
                if col.is_empty() {
                    let at = if part.is_empty() {
                        part.start..part.start
                    } else {
                        part
                    };
                    return Err(QueryError::new("expected a column", at));
                }
                let c = resolve_column(input, col, &names)?;
                if !keys.contains(&c) {
                    keys.push(c);
                }
            }
        }
        Ok(DupeSpec {
            columns: keys,
            mode,
        })
    }

    /// The key as text: `sku, email`, or `all columns`.
    pub fn key_text(&self, columns: &[ColumnMeta]) -> String {
        if self.columns.is_empty() {
            return "all columns".to_owned();
        }
        self.columns
            .iter()
            .map(|&c| match columns.get(c) {
                Some(m) => ColumnRef::Name {
                    name: m.name.query.clone(),
                    span: 0..0,
                }
                .to_string(),
                None => format!("${}", c + 1),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The canonical command text: `dupes sku, email` or `dedupe`.
    pub fn display(&self, columns: &[ColumnMeta]) -> String {
        if self.columns.is_empty() {
            self.mode.command().to_owned()
        } else {
            format!("{} {}", self.mode.command(), self.key_text(columns))
        }
    }
}

/// Tuning knobs, mostly for tests.
#[derive(Clone, Debug)]
pub struct DupeOptions {
    /// Parent positions per extraction chunk (default 64 Ki).
    pub chunk_rows: u64,
    /// Number of partitions; `None` = from the budget. Tests force spills.
    pub partitions: Option<usize>,
}

impl Default for DupeOptions {
    fn default() -> DupeOptions {
        DupeOptions {
            chunk_rows: 64 * 1024,
            partitions: None,
        }
    }
}

/// Everything a duplicates job needs.
#[derive(Clone, Debug)]
pub struct DupeJob {
    /// The file, with the column edits to compare with.
    pub src: Arc<Source>,
    /// Its row index.
    pub index: Arc<RowIndex>,
    /// The view being searched: `All`, `Filtered` or `Ordered`.
    pub parent: View,
    /// Key field positions (`source_index`); empty = every field.
    pub fields: Vec<usize>,
    /// Show or remove.
    pub mode: DupeMode,
    /// Memory for the records, from the job budget (§10.4).
    pub ram_cap: u64,
    /// `--tmp`.
    pub tmp_dir: PathBuf,
    /// Tuning.
    pub options: DupeOptions,
}

/// The result view's rows.
#[derive(Clone, Debug)]
pub enum DupeRows {
    /// For an `All` or `Filtered` parent: row ids in file order.
    Rows(Arc<FilterRows>),
    /// For an `Ordered` parent: row ids in the parent's order.
    List(Arc<RowIdList>),
}

impl DupeRows {
    /// Number of rows.
    pub fn len(&self) -> u64 {
        match self {
            DupeRows::Rows(r) => r.len(),
            DupeRows::List(l) => l.len(),
        }
    }

    /// No rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a finished job delivers.
#[derive(Clone, Debug)]
pub struct DupeResult {
    /// The selected rows.
    pub rows: DupeRows,
    /// Keys that occur 2+ times.
    pub groups: u64,
    /// Rows of the parent view.
    pub parent_rows: u64,
}

impl DupeResult {
    /// The toast text: `12 duplicate rows in 5 groups` /
    /// `removed 7 duplicate rows (5 groups)`.
    pub fn summary(&self, mode: DupeMode) -> String {
        use crate::size::format_count;
        let n = self.rows.len();
        match mode {
            DupeMode::Show if n == 0 => "no duplicate rows".to_owned(),
            DupeMode::Show => format!(
                "{} duplicate rows in {} {}",
                format_count(n),
                format_count(self.groups),
                plural(self.groups, "group", "groups"),
            ),
            DupeMode::Remove => {
                let removed = self.parent_rows.saturating_sub(n);
                if removed == 0 {
                    "no duplicate rows to remove".to_owned()
                } else {
                    format!(
                        "removed {} duplicate {} ({} {})",
                        format_count(removed),
                        plural(removed, "row", "rows"),
                        format_count(self.groups),
                        plural(self.groups, "group", "groups"),
                    )
                }
            }
        }
    }
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// One extracted row: its key hash and view position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct KeyRec {
    hash: u128,
    pos: u64,
}

/// Bytes of a [`KeyRec`] in memory and in a partition file.
const REC_BYTES: u64 = 24;

impl KeyRec {
    fn to_bytes(self) -> [u8; REC_BYTES as usize] {
        let mut b = [0; REC_BYTES as usize];
        b[..16].copy_from_slice(&self.hash.to_le_bytes());
        b[16..].copy_from_slice(&self.pos.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; REC_BYTES as usize]) -> KeyRec {
        KeyRec {
            hash: u128::from_le_bytes(b[..16].try_into().expect("16 bytes")),
            pos: u64::from_le_bytes(b[16..].try_into().expect("8 bytes")),
        }
    }
}

/// Hashes keys of records parsed from one source.
struct KeyHasher<'a> {
    src: &'a Source,
    fields: &'a [usize],
    parser: RecordParser,
    scratch: Vec<u8>,
    edited: Vec<u8>,
}

impl<'a> KeyHasher<'a> {
    fn new(src: &'a Source, fields: &'a [usize]) -> KeyHasher<'a> {
        KeyHasher {
            src,
            fields,
            parser: RecordParser::new(src.dialect()),
            scratch: Vec::new(),
            edited: Vec::new(),
        }
    }

    /// The 128-bit hash of `rec`'s key.
    fn hash(&mut self, rec: &RecordRanges) -> u128 {
        let mut a = DefaultHasher::new();
        let mut b = DefaultHasher::new();
        b.write_u8(0xA5);
        let all = self.fields.is_empty();
        let n = if all {
            rec.fields.len().max(self.src.width())
        } else {
            self.fields.len()
        };
        for k in 0..n {
            let field = if all { k } else { self.fields[k] };
            let v = if field < rec.fields.len() {
                let v = self
                    .parser
                    .field_value(self.src.bytes(), rec, field, &mut self.scratch);
                self.src.edits().apply(field, v, &mut self.edited)
            } else {
                &[]
            };
            for h in [&mut a, &mut b] {
                h.write_u64(v.len() as u64);
                h.write(v);
            }
        }
        (u128::from(a.finish()) << 64) | u128::from(b.finish())
    }
}

/// Hashes parent positions `first .. first + count`.
fn extract_chunk(
    src: &Source,
    index: &RowIndex,
    parent: &View,
    fields: &[usize],
    first: u64,
    count: u64,
    ctl: &JobControl,
) -> Result<Vec<KeyRec>, JobError> {
    ctl.check()?;
    let mut ids: Vec<(u64, u64)> = parent
        .row_ids(first, count as usize)
        .into_iter()
        .enumerate()
        .map(|(i, id)| (id, first + i as u64))
        .collect();
    // Ascending ids turn an `Ordered` parent's random seeks into a forward
    // walk; the position travels with each record.
    if matches!(parent, View::Ordered { .. }) {
        ids.sort_unstable();
    }
    let mut seeker = RowSeeker::new(src);
    let mut hasher = KeyHasher::new(src, fields);
    let mut rec = RecordRanges::default();
    let mut out = Vec::with_capacity(ids.len());
    let mut since_check = 0u64;
    for (id, pos) in ids {
        let Some(n) = seeker.parse(src, index, id, &mut rec) else {
            continue;
        };
        out.push(KeyRec {
            hash: hasher.hash(&rec),
            pos,
        });
        since_check += n;
        if since_check >= CHECK_INTERVAL as u64 {
            since_check = 0;
            ctl.check()?;
        }
    }
    Ok(out)
}

/// Where extracted records go.
enum Sink {
    Memory(Vec<KeyRec>),
    Files {
        writers: Vec<BufWriter<File>>,
        paths: Vec<PathBuf>,
        /// Kept alive (and removed on drop) for the job's duration.
        _dir: tempfile::TempDir,
    },
}

impl Sink {
    fn push(&mut self, recs: &[KeyRec], shift: u32) -> Result<(), JobError> {
        match self {
            Sink::Memory(v) => v.extend_from_slice(recs),
            Sink::Files { writers, paths, .. } => {
                for r in recs {
                    let p = (r.hash >> shift) as usize;
                    writers[p]
                        .write_all(&r.to_bytes())
                        .map_err(|e| JobError::io(format!("writing {}", paths[p].display()), e))?;
                }
            }
        }
        Ok(())
    }
}

/// Selects positions of one sorted partition into `selected`; returns the
/// number of keys that occur 2+ times.
fn group(recs: &mut [KeyRec], mode: DupeMode, selected: &mut RoaringTreemap) -> u64 {
    recs.sort_unstable();
    let mut groups = 0;
    for run in recs.chunk_by(|a, b| a.hash == b.hash) {
        if run.len() > 1 {
            groups += 1;
        }
        match mode {
            DupeMode::Remove => {
                selected.insert(run[0].pos);
            }
            DupeMode::Show if run.len() > 1 => selected.extend(run.iter().map(|r| r.pos)),
            DupeMode::Show => {}
        }
    }
    groups
}

/// Reads a partition file back.
fn read_partition(path: &PathBuf) -> Result<Vec<KeyRec>, JobError> {
    let ctx = || format!("reading {}", path.display());
    let file = File::open(path).map_err(|e| JobError::io(ctx(), e))?;
    let len = file.metadata().map_err(|e| JobError::io(ctx(), e))?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut out = Vec::with_capacity((len / REC_BYTES) as usize);
    let mut buf = [0u8; REC_BYTES as usize];
    for _ in 0..len / REC_BYTES {
        r.read_exact(&mut buf).map_err(|e| JobError::io(ctx(), e))?;
        out.push(KeyRec::from_bytes(&buf));
    }
    Ok(out)
}

/// Runs a `dupes` / `dedupe` job. See the module docs.
///
/// - Waits (polling every 100 ms) while the parent is still growing.
/// - `ctl` is checked every 64 KiB of input and between partitions.
/// - Any I/O error (disk full) → [`JobError::Io`]; the job's files are
///   removed, the parent view is unaffected.
///
/// `progress` counts parent rows twice: once when extracted, once when
/// grouped.
pub async fn run_dupes(
    job: DupeJob,
    exec: &Executor,
    ctl: &JobControl,
    progress: Arc<RowsProgress>,
) -> Result<DupeResult, JobError> {
    while job.parent.is_growing(&job.index) {
        if ctl.cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rows = job.parent.len(&job.index);
    progress
        .total
        .store(rows.saturating_mul(2), std::sync::atomic::Ordering::Relaxed);

    // Partitions: a power of two, so the top hash bits pick one.
    let want = job.options.partitions.unwrap_or_else(|| {
        let bytes = rows.saturating_mul(REC_BYTES);
        bytes.div_ceil(job.ram_cap.max(1 << 20)).max(1) as usize
    });
    let parts = want.clamp(1, 1024).next_power_of_two();
    let bits = parts.trailing_zeros();
    let shift = 128 - bits;
    let mut sink = if parts == 1 {
        Sink::Memory(Vec::with_capacity(rows as usize))
    } else {
        check_space(&job.tmp_dir, rows.saturating_mul(REC_BYTES))?;
        let dir = tempfile::Builder::new()
            .prefix("tachy-job-")
            .tempdir_in(&job.tmp_dir)
            .map_err(|e| {
                JobError::io(
                    format!("creating a temp dir in {}", job.tmp_dir.display()),
                    e,
                )
            })?;
        let mut writers = Vec::with_capacity(parts);
        let mut paths = Vec::with_capacity(parts);
        for p in 0..parts {
            let path = dir.path().join(format!("part-{p}"));
            let f = File::create(&path)
                .map_err(|e| JobError::io(format!("creating {}", path.display()), e))?;
            writers.push(BufWriter::with_capacity(64 << 10, f));
            paths.push(path);
        }
        Sink::Files {
            writers,
            paths,
            _dir: dir,
        }
    };
    let _scan = job.src.begin_scan();

    // Phase 1: extraction, a bounded window of chunk tasks.
    let fields: Arc<[usize]> = job.fields.clone().into();
    let window = exec.threads().max(1) * 2;
    let chunk = job.options.chunk_rows.max(1);
    let mut next = 0u64;
    let mut set: JoinSet<Result<Vec<KeyRec>, JobError>> = JoinSet::new();
    let mut failure: Option<JobError> = None;
    loop {
        while failure.is_none() && set.len() < window && next < rows {
            let count = chunk.min(rows - next);
            let (src, index, parent, fields, ctl, exec) = (
                Arc::clone(&job.src),
                Arc::clone(&job.index),
                job.parent.clone(),
                Arc::clone(&fields),
                ctl.clone(),
                exec.clone(),
            );
            let first = next;
            set.spawn(async move {
                exec.run(move || extract_chunk(&src, &index, &parent, &fields, first, count, &ctl))
                    .await
            });
            next += count;
        }
        let Some(done) = set.join_next().await else {
            break;
        };
        match done {
            Ok(Ok(recs)) if failure.is_none() => {
                progress.add(recs.len() as u64);
                if let Err(e) = sink.push(&recs, shift) {
                    failure = Some(e);
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                failure.get_or_insert(e);
            }
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => {
                failure.get_or_insert(JobError::Cancelled);
            }
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }

    // Phase 2: group each partition.
    let (selected, groups) = match sink {
        Sink::Memory(mut recs) => {
            let ctl2 = ctl.clone();
            let progress2 = Arc::clone(&progress);
            let mode = job.mode;
            exec.run(move || {
                ctl2.check()?;
                let mut selected = RoaringTreemap::new();
                let groups = group(&mut recs, mode, &mut selected);
                progress2.add(recs.len() as u64);
                Ok::<_, JobError>((selected, groups))
            })
            .await?
        }
        Sink::Files {
            writers,
            paths,
            _dir,
        } => {
            for (mut w, p) in writers.into_iter().zip(&paths) {
                w.flush()
                    .map_err(|e| JobError::io(format!("writing {}", p.display()), e))?;
            }
            let mut selected = RoaringTreemap::new();
            let mut groups = 0;
            let mut tasks: JoinSet<Result<(RoaringTreemap, u64), JobError>> = JoinSet::new();
            let mut pending = paths.into_iter();
            loop {
                while tasks.len() < exec.threads().max(1)
                    && let Some(path) = pending.next()
                {
                    let (ctl, exec, progress, mode) =
                        (ctl.clone(), exec.clone(), Arc::clone(&progress), job.mode);
                    tasks.spawn(async move {
                        exec.run(move || {
                            ctl.check()?;
                            let mut recs = read_partition(&path)?;
                            let mut sel = RoaringTreemap::new();
                            let g = group(&mut recs, mode, &mut sel);
                            progress.add(recs.len() as u64);
                            Ok((sel, g))
                        })
                        .await
                    });
                }
                let Some(done) = tasks.join_next().await else {
                    break;
                };
                match done {
                    Ok(Ok((sel, g))) => {
                        selected |= sel;
                        groups += g;
                    }
                    Ok(Err(e)) => return Err(e),
                    Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                    Err(_) => return Err(JobError::Cancelled),
                }
            }
            drop(_dir);
            (selected, groups)
        }
    };
    ctl.check()?;

    // Phase 3: positions → row ids, in view order.
    let out = match &job.parent {
        View::All => DupeRows::Rows(Arc::new(FilterRows::from_bitmap(selected))),
        View::Filtered { rows: parent, .. } | View::Dupes { rows: parent, .. } => {
            let ids = parent.with_bitmap(|b| {
                b.iter()
                    .enumerate()
                    .filter(|(pos, _)| selected.contains(*pos as u64))
                    .map(|(_, id)| id)
                    .collect::<RoaringTreemap>()
            });
            DupeRows::Rows(Arc::new(FilterRows::from_bitmap(ids)))
        }
        View::Ordered { list, .. } => {
            let (_, mut writer) = RowIdList::create_in(&job.tmp_dir).map_err(|e| {
                JobError::io(
                    format!("creating the row list in {}", job.tmp_dir.display()),
                    e,
                )
            })?;
            let ctx = format!("writing {}", writer.list().path().display());
            const BATCH: usize = 64 * 1024;
            let mut first = 0u64;
            while first < rows {
                ctl.check()?;
                let ids = list.read(first, BATCH);
                if ids.is_empty() {
                    break;
                }
                let keep: Vec<u64> = ids
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| selected.contains(first + *i as u64))
                    .map(|(_, &id)| id)
                    .collect();
                writer
                    .extend(&keep)
                    .map_err(|e| JobError::io(ctx.clone(), e))?;
                first += ids.len() as u64;
            }
            let list = writer.finish().map_err(|e| JobError::io(ctx, e))?;
            DupeRows::List(list)
        }
    };
    Ok(DupeResult {
        rows: out,
        groups,
        parent_rows: rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(hash: u128, pos: u64) -> KeyRec {
        KeyRec { hash, pos }
    }

    #[test]
    fn key_rec_round_trips() {
        let r = rec(u128::MAX - 7, 1 << 40);
        assert_eq!(KeyRec::from_bytes(&r.to_bytes()), r);
    }

    #[test]
    fn group_show_selects_whole_groups() {
        let mut recs = vec![
            rec(5, 3),
            rec(1, 0),
            rec(5, 1),
            rec(2, 2),
            rec(5, 7),
            rec(2, 4),
        ];
        let mut sel = RoaringTreemap::new();
        assert_eq!(group(&mut recs, DupeMode::Show, &mut sel), 2);
        assert_eq!(sel.iter().collect::<Vec<_>>(), [1, 2, 3, 4, 7]);
    }

    #[test]
    fn group_remove_keeps_first_position() {
        let mut recs = vec![
            rec(5, 3),
            rec(1, 0),
            rec(5, 1),
            rec(2, 2),
            rec(5, 7),
            rec(2, 4),
        ];
        let mut sel = RoaringTreemap::new();
        assert_eq!(group(&mut recs, DupeMode::Remove, &mut sel), 2);
        assert_eq!(sel.iter().collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn summaries() {
        let r = |n: u64, groups: u64, parent_rows: u64| DupeResult {
            rows: DupeRows::Rows(Arc::new(FilterRows::from_bitmap((0..n).collect()))),
            groups,
            parent_rows,
        };
        assert_eq!(r(0, 0, 9).summary(DupeMode::Show), "no duplicate rows");
        assert_eq!(
            r(5, 2, 9).summary(DupeMode::Show),
            "5 duplicate rows in 2 groups"
        );
        assert_eq!(
            r(2, 1, 9).summary(DupeMode::Show),
            "2 duplicate rows in 1 group"
        );
        assert_eq!(
            r(9, 0, 9).summary(DupeMode::Remove),
            "no duplicate rows to remove"
        );
        assert_eq!(
            r(8, 1, 9).summary(DupeMode::Remove),
            "removed 1 duplicate row (1 group)"
        );
    }
}
