//! Export of a view (or a column subset) to a new delimited file (spec §8.5;
//! M6-01).
//!
//! # Output format
//!
//! - Rows are written **in view order** (file order for `All` and filtered
//!   views, the permutation order for sorted views).
//! - Field values are the **unescaped raw bytes** of the source (§16): no
//!   `�` replacement, no control-character escaping, no transcoding. The
//!   output encoding is the source encoding (UTF-8 for a transcoded UTF-16
//!   source, Windows-1252 bytes for a Windows-1252 source). A UTF-8 byte
//!   order mark in the source is **not** written.
//! - Quoting always uses `"` with doubled-quote escaping, whatever the
//!   source dialect (a backslash-escaped or single-quoted source is
//!   normalised). See [`format_record`] for when fields are quoted.
//! - Records end with `\n`, always (never `\r\n`).
//! - The header row (optional) holds the columns' **display** names, quoted
//!   like any field (encoded to Windows-1252 for such sources).
//! - Ragged rows (§6.4): exactly the selected columns are written. Missing
//!   cells of short rows are empty fields, `_extraN` columns are written when
//!   selected, so every output row has the same field count.
//!
//! # Atomicity
//!
//! The file is written to `<path>.partial` in the target directory (so the
//! final `rename` is atomic), then `sync_all`ed and renamed to `<path>`. On
//! cancellation or failure (disk full, §16) an RAII guard deletes the
//! partial file: the view is unaffected and an existing target is
//! untouched.
//!
//! # Parallelism
//!
//! `All` views are cut into checkpoint-aligned chunks, `Filtered` and
//! `Ordered` views into batches of positions. Chunks are formatted in
//! parallel into `Vec<u8>` buffers ([`Executor::run`]) and written **in
//! order** through a [`ReorderBuffer`]. At most `threads × 2` buffers are
//! alive at once; their size is derived from the job's budget (see
//! [`ExportOptions::for_budget`]).

use std::{
    fmt,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio::task::JoinSet;

use crate::{
    column::ColumnMeta,
    dialect::Encoding,
    exec::Executor,
    filter::{ParentRows, RangePlanner, ReorderBuffer, RowRange, RowSeeker, Ticker},
    index::RowIndex,
    jobs::{EXPORT_BUDGET, ExportProgress, JobControl, JobError},
    parse::{ParseOutcome, RecordParser, RecordRanges},
    source::Source,
};

/// The rows of the exported view: the same shapes as a filter's parent.
pub type ExportRows = ParentRows;

/// Largest `BufWriter` capacity (8 MiB).
pub const MAX_WRITE_BUFFER: usize = 8 << 20;

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// How fields are quoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Quoting {
    /// Only fields that need it (see [`needs_quotes`]).
    #[default]
    Minimal,
    /// Every field.
    All,
}

/// Whether `minimal` quoting quotes `field`: it contains the delimiter, `"`,
/// `\r` or `\n`, or the delimiter is a space and the field starts or ends
/// with a space.
pub fn needs_quotes(field: &[u8], delim: u8) -> bool {
    field
        .iter()
        .any(|&b| b == delim || b == b'"' || b == b'\r' || b == b'\n')
        || (delim == b' ' && (field.first() == Some(&b' ') || field.last() == Some(&b' ')))
}

fn push_field(field: &[u8], delim: u8, quoting: Quoting, out: &mut Vec<u8>) {
    if quoting == Quoting::All || needs_quotes(field, delim) {
        out.push(b'"');
        let mut rest = field;
        while let Some(i) = memchr::memchr(b'"', rest) {
            out.extend_from_slice(&rest[..=i]);
            out.push(b'"');
            rest = &rest[i + 1..];
        }
        out.extend_from_slice(rest);
        out.push(b'"');
    } else {
        out.extend_from_slice(field);
    }
}

/// Appends one record to `out`: `fields` separated by `delim`, quoted per
/// `quoting` with `"` and doubled-quote escaping. **No** line terminator is
/// written (the export appends `\n`; the clipboard joins rows itself).
///
/// A record made of a single empty field is written as `""` even with
/// `minimal` quoting, as the `csv` crate does: an empty line would be read
/// back as a blank line, not as a row.
pub fn format_record(fields: &[&[u8]], delim: u8, quoting: Quoting, out: &mut Vec<u8>) {
    if let [only] = fields
        && only.is_empty()
    {
        out.extend_from_slice(b"\"\"");
        return;
    }
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(delim);
        }
        push_field(f, delim, quoting, out);
    }
}

// ---------------------------------------------------------------------------
// Options and targets
// ---------------------------------------------------------------------------

/// One exported column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportColumn {
    /// Field position in the record (`ColumnMeta::source_index`).
    pub field: usize,
    /// Header name (the display name).
    pub name: String,
}

impl ExportColumn {
    /// The column described by `meta`.
    pub fn from_meta(meta: &ColumnMeta) -> ExportColumn {
        ExportColumn {
            field: meta.source_index,
            name: meta.name.display.clone(),
        }
    }

    /// The dialog's `Columns` choice: `visible` → the visible columns in
    /// display order (`display` lists column indices); `all` → every column
    /// in source order (`_extraN` included).
    pub fn select(cols: &[ColumnMeta], display: &[usize], visible: bool) -> Vec<ExportColumn> {
        if visible {
            display
                .iter()
                .filter_map(|&i| cols.get(i).map(ExportColumn::from_meta))
                .collect()
        } else {
            let mut all: Vec<&ColumnMeta> = cols.iter().collect();
            all.sort_by_key(|c| c.source_index);
            all.into_iter().map(ExportColumn::from_meta).collect()
        }
    }
}

/// Export settings (the dialog's fields) and tuning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportOptions {
    /// Output delimiter (default: the source's).
    pub delimiter: u8,
    /// Quoting mode.
    pub quoting: Quoting,
    /// Write a header row.
    pub header: bool,
    /// `BufWriter` capacity: `min(8 MiB, budget)`.
    pub write_buffer: usize,
    /// Nominal chunk size of an `All` view (bytes of input).
    pub chunk_bytes: u64,
    /// Positions per batch of a `Filtered` / `Ordered` view (at most 64k).
    pub batch_rows: u64,
}

impl ExportOptions {
    /// Options sized from the job's memory grant (M5-01: [`EXPORT_BUDGET`],
    /// 32 MiB): the write buffer is `min(8 MiB, budget)`; the
    /// `threads × 2` format buffers share the budget, so a chunk is
    /// `clamp(budget / (threads × 2), 1 MiB, 64 MiB)` of input, and a batch
    /// is `clamp(chunk / avg_record_len, 1,024, 65,536)` positions.
    pub fn for_budget(
        delimiter: u8,
        quoting: Quoting,
        header: bool,
        budget: u64,
        threads: usize,
        avg_record_len: u64,
    ) -> ExportOptions {
        let chunk = (budget / (threads.max(1) as u64 * 2)).clamp(1 << 20, 64 << 20);
        ExportOptions {
            delimiter,
            quoting,
            header,
            write_buffer: usize::try_from(budget)
                .unwrap_or(usize::MAX)
                .min(MAX_WRITE_BUFFER),
            chunk_bytes: chunk,
            batch_rows: (chunk / avg_record_len.max(1)).clamp(1024, 65_536),
        }
    }

    /// [`ExportOptions::for_budget`] with [`EXPORT_BUDGET`], the source's
    /// delimiter, minimal quoting and its header setting, for `src`.
    pub fn defaults_for(src: &Source, index: &RowIndex, threads: usize) -> ExportOptions {
        let rows = index.indexed_rows().max(1);
        let avg = (src.len().saturating_sub(src.data_start()) / rows).max(1);
        ExportOptions::for_budget(
            src.dialect().delimiter,
            Quoting::Minimal,
            src.dialect().header,
            EXPORT_BUDGET,
            threads,
            avg,
        )
    }
}

/// What the export job needs besides its inputs.
#[derive(Clone, Debug)]
pub struct ExportContext {
    /// The app's executor.
    pub exec: Executor,
    /// The job's tokens.
    pub ctl: JobControl,
    /// Rows and bytes written.
    pub progress: Arc<ExportProgress>,
}

/// Result of a finished export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportSummary {
    /// Data rows written (header excluded), for the
    /// `exported 1,234,567 rows to orders.filtered.csv` toast.
    pub rows: u64,
    /// Bytes written, header included.
    pub bytes: u64,
    /// The final path.
    pub path: PathBuf,
}

/// `<path>.partial`, next to the target.
pub fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    target.with_file_name(name)
}

/// The dialog's default path: `<source dir>/<source stem>.<view>.csv`
/// (`orders.filtered.csv`), or `<cwd>/stdin.<view>.csv` for stdin
/// (`source = None`). `view` is `all`, `filtered` or `sorted`.
pub fn default_target(source: Option<&Path>, view: &str, cwd: &Path) -> PathBuf {
    match source {
        Some(p) => {
            let stem = p.file_stem().unwrap_or_default().to_string_lossy();
            let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
            dir.unwrap_or(cwd).join(format!("{stem}.{view}.csv"))
        }
        None => cwd.join(format!("stdin.{view}.csv")),
    }
}

/// Why a target path is refused (inline errors of the dialog).
#[derive(Debug, Error)]
pub enum TargetError {
    /// The target is the source file (§1), possibly through a symlink, a
    /// hard link or a `./` prefix.
    #[error("refusing to overwrite the source file")]
    SourceFile,
    /// The path has no file name.
    #[error("not a file path")]
    NoFileName,
    /// The target is a directory.
    #[error("{} is a directory", .0.display())]
    IsDirectory(PathBuf),
    /// The target directory does not exist.
    #[error("directory {} does not exist", .0.display())]
    NoDirectory(PathBuf),
    /// The target directory is not writable.
    #[error("cannot write to {}: {source}", dir.display())]
    NotWritable {
        /// The directory.
        dir: PathBuf,
        /// What creating a probe file returned.
        #[source]
        source: io::Error,
    },
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_: &Path, _: &Path) -> bool {
    false
}

/// Checks a target before the export starts (§1, §8.5):
///
/// - refuses the source file: `canonicalize(parent) / file_name` and, when
///   the target exists, `canonicalize(target)` are compared with the
///   canonical source path, and on Unix `(dev, ino)` too (hard links);
/// - the target directory must exist and be writable (a probe file is
///   created and removed).
///
/// Whether the target exists (the `overwrite …? (y/n)` line) is the UI's
/// `target.exists()`.
pub fn check_target(target: &Path, source: &Path) -> Result<(), TargetError> {
    let name = target.file_name().ok_or(TargetError::NoFileName)?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = fs::canonicalize(parent)
        .ok()
        .filter(|d| d.is_dir())
        .ok_or_else(|| TargetError::NoDirectory(parent.to_path_buf()))?;
    let joined = dir.join(name);
    let canon_source = fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    if joined == canon_source
        || fs::canonicalize(&joined).is_ok_and(|t| t == canon_source)
        || same_file(&joined, source)
    {
        return Err(TargetError::SourceFile);
    }
    if joined.is_dir() {
        return Err(TargetError::IsDirectory(joined));
    }
    tempfile::Builder::new()
        .prefix(".tachy-probe-")
        .tempfile_in(&dir)
        .map_err(|source| TargetError::NotWritable { dir, source })?;
    Ok(())
}

/// Removes `<path>.partial` on drop unless committed.
struct PartialGuard {
    path: PathBuf,
    committed: bool,
}

impl Drop for PartialGuard {
    fn drop(&mut self) {
        if !self.committed
            && let Err(e) = fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!("removing {}: {e}", self.path.display());
        }
    }
}

/// Wraps the `.partial` file in another writer (tests inject failures).
pub type WriterWrap = Box<dyn FnOnce(File) -> Box<dyn Write + Send> + Send>;

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

struct Fmt {
    src: Arc<Source>,
    index: Arc<RowIndex>,
    view: ExportRows,
    fields: Vec<usize>,
    delim: u8,
    quoting: Quoting,
}

impl Fmt {
    /// Appends the current record's selected fields and `\n`.
    fn record(
        &self,
        parser: &RecordParser,
        rec: &RecordRanges,
        scratch: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) {
        let bytes = self.src.bytes();
        if let [field] = self.fields[..] {
            let v = if field < rec.fields.len() {
                parser.field_value(bytes, rec, field, scratch)
            } else {
                &[]
            };
            format_record(&[v], self.delim, self.quoting, out);
        } else {
            for (i, &field) in self.fields.iter().enumerate() {
                if i > 0 {
                    out.push(self.delim);
                }
                if field < rec.fields.len() {
                    let v = parser.field_value(bytes, rec, field, scratch);
                    push_field(v, self.delim, self.quoting, out);
                } else if self.quoting == Quoting::All {
                    out.extend_from_slice(b"\"\"");
                }
            }
        }
        out.push(b'\n');
    }

    fn range(&self, r: RowRange, ctl: &JobControl) -> Result<(Vec<u8>, u64), JobError> {
        let bytes = self.src.bytes();
        let mut parser = RecordParser::new(self.src.dialect());
        let mut rec = RecordRanges::default();
        let mut scratch = Vec::new();
        let mut ticker = Ticker::new(ctl);
        let mut out = Vec::with_capacity((r.end - r.start) as usize + 1024);
        let mut pos = r.start;
        let mut rows = 0;
        for _ in 0..r.rows {
            let next = match parser.parse_at(bytes, pos, &mut rec) {
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
                ParseOutcome::Eof => break,
            };
            self.record(&parser, &rec, &mut scratch, &mut out);
            rows += 1;
            ticker.tick(next - pos)?;
            pos = next;
        }
        Ok((out, rows))
    }

    fn ids(&self, first: u64, count: u64, ctl: &JobControl) -> Result<(Vec<u8>, u64), JobError> {
        let ids = self.view.row_ids(first, count as usize);
        let mut seeker = RowSeeker::new(&self.src);
        let mut rec = RecordRanges::default();
        let mut scratch = Vec::new();
        let mut ticker = Ticker::new(ctl);
        let mut out = Vec::new();
        let mut rows = 0;
        for &id in &ids {
            let Some(n) = seeker.parse(&self.src, &self.index, id, &mut rec) else {
                continue;
            };
            self.record(&seeker.parser, &rec, &mut scratch, &mut out);
            rows += 1;
            ticker.tick(n)?;
        }
        Ok((out, rows))
    }
}

/// A formatted chunk: `(index, bytes, rows)`.
type Formatted = (usize, Vec<u8>, u64);

enum Item {
    Range(RowRange),
    Ids { first: u64, count: u64 },
}

/// Runs an export job (M6-01). See the module docs.
pub async fn run_export(
    view: ExportRows,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    cols: Vec<ExportColumn>,
    opts: ExportOptions,
    target: PathBuf,
    ctx: ExportContext,
) -> Result<ExportSummary, JobError> {
    run_export_with(view, src, index, cols, opts, target, ctx, None).await
}

/// Writes `buf` through the writer on the blocking pool.
async fn write_out(
    w: &mut Option<BufWriter<Box<dyn Write + Send>>>,
    buf: Vec<u8>,
    what: &Path,
) -> Result<(), JobError> {
    let mut writer = w.take().expect("the writer is put back after each write");
    let (writer, r) = tokio::task::spawn_blocking(move || {
        let r = writer.write_all(&buf);
        (writer, r)
    })
    .await
    .map_err(|e| JobError::Other(format!("export write task failed: {e}")))?;
    *w = Some(writer);
    r.map_err(|e| JobError::io(format!("writing {}", what.display()), e))
}

/// [`run_export`] with an optional [`WriterWrap`] around the `.partial` file.
#[allow(clippy::too_many_arguments)]
pub async fn run_export_with(
    view: ExportRows,
    src: Arc<Source>,
    index: Arc<RowIndex>,
    cols: Vec<ExportColumn>,
    opts: ExportOptions,
    target: PathBuf,
    ctx: ExportContext,
    wrap: Option<WriterWrap>,
) -> Result<ExportSummary, JobError> {
    let ExportContext {
        exec,
        ctl,
        progress,
    } = ctx;
    // A growing view (running filter, index still building) is exported once
    // it is complete, as the sort does.
    while view.is_growing(&index) {
        tokio::select! {
            _ = ctl.cancel.cancelled() => return Err(JobError::Cancelled),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    let total = view.len(&index);
    progress
        .total_rows
        .store(total, std::sync::atomic::Ordering::Relaxed);

    let partial = partial_path(&target);
    let file = File::create(&partial)
        .map_err(|e| JobError::io(format!("creating {}", partial.display()), e))?;
    let mut guard = PartialGuard {
        path: partial.clone(),
        committed: false,
    };
    let sync_handle = file
        .try_clone()
        .map_err(|e| JobError::io(format!("opening {}", partial.display()), e))?;
    let inner: Box<dyn Write + Send> = match wrap {
        Some(wrap) => wrap(file),
        None => Box::new(file),
    };
    let mut writer = Some(BufWriter::with_capacity(opts.write_buffer.max(4096), inner));
    let _scan = src.begin_scan();
    let mut bytes_written = 0u64;
    let mut rows_written = 0u64;

    if opts.header {
        let names: Vec<Vec<u8>> = cols
            .iter()
            .map(|c| match src.dialect().encoding {
                Encoding::Windows1252 => encoding_rs::WINDOWS_1252.encode(&c.name).0.into_owned(),
                _ => c.name.as_bytes().to_vec(),
            })
            .collect();
        let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        let mut buf = Vec::new();
        format_record(&refs, opts.delimiter, opts.quoting, &mut buf);
        buf.push(b'\n');
        bytes_written += buf.len() as u64;
        progress.add(0, buf.len() as u64);
        write_out(&mut writer, buf, &partial).await?;
    }

    let fmt = Arc::new(Fmt {
        src: Arc::clone(&src),
        index: Arc::clone(&index),
        view: view.clone(),
        fields: cols.iter().map(|c| c.field).collect(),
        delim: opts.delimiter,
        quoting: opts.quoting,
    });
    let mut planner = RangePlanner::new(opts.chunk_bytes, opts.chunk_bytes);
    let mut next_pos = 0u64;
    let mut next_item = move || -> Option<Item> {
        match &view {
            ParentRows::All => planner.next_complete(&src, &index).map(Item::Range),
            _ => {
                if next_pos >= total {
                    return None;
                }
                let count = (total - next_pos).min(opts.batch_rows.max(1));
                let first = next_pos;
                next_pos += count;
                Some(Item::Ids { first, count })
            }
        }
    };
    let window = exec.threads().max(1) * 2;
    let mut set: JoinSet<Result<Formatted, JobError>> = JoinSet::new();
    let mut reorder = ReorderBuffer::new();
    let mut next_idx = 0usize;
    let mut planned_all = false;
    loop {
        while !planned_all && set.len() + reorder.pending() < window {
            let Some(item) = next_item() else {
                planned_all = true;
                break;
            };
            let (fmt, ctl, exec) = (Arc::clone(&fmt), ctl.clone(), exec.clone());
            let idx = next_idx;
            next_idx += 1;
            set.spawn(async move {
                exec.run(move || {
                    ctl.check()?;
                    let r = match item {
                        Item::Range(r) => fmt.range(r, &ctl),
                        Item::Ids { first, count } => fmt.ids(first, count, &ctl),
                    };
                    r.map(|(buf, rows)| (idx, buf, rows))
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
        let (idx, buf, rows) = match joined {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e),
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => return Err(JobError::Other(format!("export task failed: {e}"))),
        };
        reorder.insert(idx, (buf, rows));
        while let Some((buf, rows)) = reorder.pop_ready() {
            // Not `ctl.check()`: that parks on a pause, which must never
            // happen on an async task. Workers park instead.
            if ctl.cancel.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            let n = buf.len() as u64;
            write_out(&mut writer, buf, &partial).await?;
            bytes_written += n;
            rows_written += rows;
            progress.add(rows, n);
        }
    }
    if ctl.cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }

    // Flush, sync, rename.
    let writer = writer
        .take()
        .expect("the writer is put back after each write");
    let (partial2, target2) = (partial.clone(), target.clone());
    tokio::task::spawn_blocking(move || -> Result<(), JobError> {
        let mut writer = writer;
        writer
            .flush()
            .map_err(|e| JobError::io(format!("writing {}", partial2.display()), e))?;
        drop(writer);
        sync_handle
            .sync_all()
            .map_err(|e| JobError::io(format!("syncing {}", partial2.display()), e))?;
        drop(sync_handle);
        fs::rename(&partial2, &target2).map_err(|e| {
            JobError::io(
                format!("renaming {} to {}", partial2.display(), target2.display()),
                e,
            )
        })
    })
    .await
    .map_err(|e| JobError::Other(format!("export task failed: {e}")))??;
    guard.committed = true;
    Ok(ExportSummary {
        rows: rows_written,
        bytes: bytes_written,
        path: target,
    })
}

impl fmt::Display for Quoting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Quoting::Minimal => "minimal",
            Quoting::All => "all",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(fields: &[&str], delim: u8, q: Quoting) -> String {
        let f: Vec<&[u8]> = fields.iter().map(|s| s.as_bytes()).collect();
        let mut out = Vec::new();
        format_record(&f, delim, q, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn minimal_quoting() {
        assert_eq!(fmt(&["a", "b c", ""], b',', Quoting::Minimal), "a,b c,");
        assert_eq!(fmt(&["a,b", "x"], b',', Quoting::Minimal), "\"a,b\",x");
        assert_eq!(
            fmt(&["say \"hi\""], b',', Quoting::Minimal),
            "\"say \"\"hi\"\"\""
        );
        assert_eq!(
            fmt(&["l1\nl2", "cr\r"], b',', Quoting::Minimal),
            "\"l1\nl2\",\"cr\r\""
        );
        // The delimiter decides: a comma is plain data in a TSV.
        assert_eq!(
            fmt(&["a,b", "c\td"], b'\t', Quoting::Minimal),
            "a,b\t\"c\td\""
        );
        assert_eq!(fmt(&["a|b"], b'|', Quoting::Minimal), "\"a|b\"");
        // Leading / trailing spaces only matter with a space delimiter.
        assert_eq!(fmt(&[" a", "b "], b',', Quoting::Minimal), " a,b ");
        assert_eq!(
            fmt(&[" a", "b ", "c"], b' ', Quoting::Minimal),
            "\" a\" \"b \" c"
        );
        // A lone empty field is quoted so it is not a blank line.
        assert_eq!(fmt(&[""], b',', Quoting::Minimal), "\"\"");
        assert_eq!(fmt(&[], b',', Quoting::Minimal), "");
    }

    #[test]
    fn all_quoting() {
        assert_eq!(
            fmt(&["a", "", "q\""], b';', Quoting::All),
            "\"a\";\"\";\"q\"\"\""
        );
        assert_eq!(fmt(&[""], b',', Quoting::All), "\"\"");
    }

    #[test]
    fn raw_bytes_are_kept() {
        let mut out = Vec::new();
        format_record(&[b"\xff\xfe", b"\x1b[0m"], b',', Quoting::Minimal, &mut out);
        assert_eq!(out, b"\xff\xfe,\x1b[0m");
    }

    #[test]
    fn paths() {
        assert_eq!(
            partial_path(Path::new("/d/orders.filtered.csv")),
            PathBuf::from("/d/orders.filtered.csv.partial")
        );
        assert_eq!(
            default_target(
                Some(Path::new("/data/orders.csv")),
                "filtered",
                Path::new("/cwd")
            ),
            PathBuf::from("/data/orders.filtered.csv")
        );
        assert_eq!(
            default_target(Some(Path::new("orders.tsv")), "sorted", Path::new("/cwd")),
            PathBuf::from("/cwd/orders.sorted.csv")
        );
        assert_eq!(
            default_target(None, "all", Path::new("/cwd")),
            PathBuf::from("/cwd/stdin.all.csv")
        );
    }
}
