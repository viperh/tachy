//! Sparse `RowIndex` (row id → byte offset) and the parallel, quote-aware
//! indexer that builds it (spec §5.2, §5.3).
//!
//! - [`RowIndex`] stores one checkpoint every [`INDEX_STRIDE`] rows. One
//!   writer (the indexer's stitching task) appends; any number of readers
//!   (the UI task, jobs) read without locks while it does.
//! - [`scan`] holds the chunk scanners, [`build`] the orchestration
//!   ([`build_index`]).

pub mod build;
pub mod scan;

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

pub use build::{IndexError, IndexOptions, IndexPath, IndexSummary, build_index};

use crate::{
    parse::{RecordParser, SkipOutcome},
    source::Source,
};

/// Rows between two checkpoints (§5.2).
pub const INDEX_STRIDE: u64 = 1024;

/// Rows past `indexed_rows` that [`RowIndex::locate_speculative`] accepts, so
/// the first screen renders before the indexer has published anything
/// (§12.3 step 1).
pub const SPECULATIVE_ROWS: u64 = 200;

/// Checkpoints per allocated chunk.
const CHUNK_LEN: usize = 65_536;
/// Number of chunks.
const MAX_CHUNKS: usize = 64;

/// Most checkpoints an index can hold: 4,194,304, i.e. about 4.3 billion
/// rows. Pushing more panics (a documented limit).
pub const MAX_CHECKPOINTS: u64 = (CHUNK_LEN * MAX_CHUNKS) as u64;

/// Bit flags of [`RowIndex::warnings`].
pub mod warnings {
    /// The file ends inside a quoted field (§16).
    pub const UNTERMINATED_QUOTE: u8 = 1;
}

/// Sparse row index. See the module docs.
///
/// Concurrency: an append-only chunked vector of atomics. The writer stores a
/// checkpoint, then publishes the new count with `Release`; readers load the
/// count with `Acquire` before reading checkpoints, so they never see a
/// checkpoint that is not fully written. `indexed_rows` is stored after the
/// checkpoints it depends on, with the same ordering.
#[derive(Debug)]
pub struct RowIndex {
    chunks: [OnceLock<Box<[AtomicU64]>>; MAX_CHUNKS],
    published: AtomicU64,
    indexed_rows: AtomicU64,
    bytes_scanned: AtomicU64,
    complete: AtomicBool,
    end_offset: AtomicU64,
    ragged_rows: AtomicU64,
    warnings: AtomicU8,
    stride: u64,
}

impl RowIndex {
    /// An empty index whose checkpoint 0 is `data_start`
    /// (`Source::data_start`), published right away.
    pub fn new(data_start: u64) -> Self {
        Self::with_stride(data_start, INDEX_STRIDE)
    }

    /// An index with one checkpoint every `stride` rows instead of
    /// [`INDEX_STRIDE`]. Tests use a stride of 1 so every record boundary
    /// the indexer finds becomes visible.
    pub fn with_stride(data_start: u64, stride: u64) -> Self {
        let index = RowIndex {
            chunks: std::array::from_fn(|_| OnceLock::new()),
            published: AtomicU64::new(0),
            indexed_rows: AtomicU64::new(0),
            bytes_scanned: AtomicU64::new(0),
            complete: AtomicBool::new(false),
            end_offset: AtomicU64::new(data_start),
            ragged_rows: AtomicU64::new(0),
            warnings: AtomicU8::new(0),
            stride: stride.max(1),
        };
        index.push_checkpoints(&[data_start]);
        index
    }

    /// An index for `src`: checkpoint 0 is `src.data_start()`.
    pub fn for_source(src: &Source) -> Self {
        Self::new(src.data_start())
    }

    fn slot(&self, k: u64) -> Option<&AtomicU64> {
        let chunk = self.chunks.get(k as usize / CHUNK_LEN)?.get()?;
        Some(&chunk[k as usize % CHUNK_LEN])
    }

    // ---- writer API (the indexer only) ----

    /// Appends checkpoints. Single writer only.
    ///
    /// # Panics
    ///
    /// When the index would exceed [`MAX_CHECKPOINTS`].
    pub fn push_checkpoints(&self, offsets: &[u64]) {
        let mut n = self.published.load(Ordering::Relaxed);
        for &offset in offsets {
            assert!(
                n < MAX_CHECKPOINTS,
                "row index full: more than {} rows are not supported",
                MAX_CHECKPOINTS * self.stride
            );
            let chunk = self.chunks[n as usize / CHUNK_LEN].get_or_init(|| {
                (0..CHUNK_LEN)
                    .map(|_| AtomicU64::new(0))
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            });
            chunk[n as usize % CHUNK_LEN].store(offset, Ordering::Relaxed);
            n += 1;
        }
        self.published.store(n, Ordering::Release);
    }

    /// Sets the number of rows whose start offset is known.
    pub fn set_indexed_rows(&self, rows: u64) {
        self.indexed_rows.store(rows, Ordering::Release);
    }

    /// Adds to the scanned-bytes progress counter.
    pub fn add_bytes_scanned(&self, n: u64) {
        self.bytes_scanned.fetch_add(n, Ordering::Relaxed);
    }

    /// Sets the scanned-bytes progress counter (the slow-path fallback moves
    /// it back to where re-indexing starts).
    pub fn set_bytes_scanned(&self, n: u64) {
        self.bytes_scanned.store(n, Ordering::Relaxed);
    }

    /// Sets the ragged-row count.
    pub fn set_ragged_rows(&self, n: u64) {
        self.ragged_rows.store(n, Ordering::Relaxed);
    }

    /// Raises a [`warnings`] flag.
    pub fn set_warning(&self, flag: u8) {
        self.warnings.fetch_or(flag, Ordering::Relaxed);
    }

    /// Marks the index complete with `total_rows` rows ending at
    /// `end_offset`.
    pub fn finish(&self, total_rows: u64, end_offset: u64) {
        self.end_offset.store(end_offset, Ordering::Relaxed);
        self.indexed_rows.store(total_rows, Ordering::Release);
        self.complete.store(true, Ordering::Release);
    }

    /// Drops everything from row `rows` on: keeps the checkpoints of rows
    /// below `rows` (checkpoint 0 always stays) and lowers `indexed_rows`.
    /// Used when re-indexing from a known-good point.
    pub fn truncate_to(&self, rows: u64) {
        self.complete.store(false, Ordering::Release);
        let keep = rows.div_ceil(self.stride).max(1);
        let rows = self.indexed_rows.load(Ordering::Relaxed).min(rows);
        self.indexed_rows.store(rows, Ordering::Release);
        let published = self.published.load(Ordering::Relaxed);
        self.published.store(published.min(keep), Ordering::Release);
    }

    // ---- reader API ----

    /// Number of readable checkpoints.
    pub fn published_checkpoints(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    /// Rows between two checkpoints ([`INDEX_STRIDE`] unless built with
    /// [`RowIndex::with_stride`]).
    pub fn stride(&self) -> u64 {
        self.stride
    }

    /// Checkpoint `k`: the offset of row `k * stride` (checkpoint 0 is
    /// `data_start`, which may precede blank or comment lines).
    pub fn checkpoint(&self, k: u64) -> Option<u64> {
        if k >= self.published_checkpoints() {
            return None;
        }
        self.slot(k).map(|s| s.load(Ordering::Relaxed))
    }

    /// Rows whose start offset is known: the lower bound shown while
    /// indexing (§5.2).
    pub fn indexed_rows(&self) -> u64 {
        self.indexed_rows.load(Ordering::Acquire)
    }

    /// Bytes scanned so far (progress gauge).
    pub fn bytes_scanned(&self) -> u64 {
        self.bytes_scanned.load(Ordering::Relaxed)
    }

    /// True once the indexer finished.
    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// The exact row count, once complete.
    pub fn total_rows(&self) -> Option<u64> {
        self.is_complete().then(|| self.indexed_rows())
    }

    /// End of the last record, without its trailing newline. Meaningful once
    /// complete.
    pub fn end_offset(&self) -> u64 {
        self.end_offset.load(Ordering::Relaxed)
    }

    /// Ragged rows counted by the indexer (exact once complete, §6.4).
    pub fn ragged_rows(&self) -> u64 {
        self.ragged_rows.load(Ordering::Relaxed)
    }

    /// [`warnings`] flags.
    pub fn warnings(&self) -> u8 {
        self.warnings.load(Ordering::Relaxed)
    }

    /// The file ends inside a quoted field.
    pub fn unterminated_quote(&self) -> bool {
        self.warnings() & warnings::UNTERMINATED_QUOTE != 0
    }

    /// `(checkpoint offset, rows to skip)` for `row`, or `None` when
    /// `row >= indexed_rows`.
    pub fn locate(&self, row: u64) -> Option<(u64, u64)> {
        if row >= self.indexed_rows() {
            return None;
        }
        let offset = self.checkpoint(row / self.stride)?;
        Some((offset, row % self.stride))
    }

    /// Like [`RowIndex::locate`], but also accepts rows up to
    /// [`SPECULATIVE_ROWS`] past `indexed_rows` while indexing, measured from
    /// the last published checkpoint (the skip may then exceed 1,023). The
    /// row may turn out to be past EOF. Only the row cache uses this; jobs
    /// never do.
    pub fn locate_speculative(&self, row: u64) -> Option<(u64, u64)> {
        let indexed = self.indexed_rows();
        if row < indexed {
            return self.locate(row);
        }
        if self.is_complete() || row >= indexed + SPECULATIVE_ROWS {
            return None;
        }
        let published = self.published_checkpoints();
        let k = (row / self.stride).min(published.saturating_sub(1));
        Some((self.checkpoint(k)?, row - k * self.stride))
    }

    /// Start offset of `row` (after any blank or comment lines before it):
    /// `locate`, then a forward skip of at most 1,023 records.
    pub fn offset_of(&self, row: u64, src: &Source, p: &mut RecordParser) -> Option<u64> {
        let (offset, skip) = self.locate(row)?;
        seek(src.bytes(), p, offset, skip)
    }

    /// `(row, start offset)` of rows `first .. first + count`, stopping at
    /// the end of the indexed range or EOF. Seeks once, then walks forward,
    /// so the skip cost is paid once per run (§5.2, §6.3).
    pub fn offsets_for_run<'a>(
        &self,
        first: u64,
        count: u64,
        src: &'a Source,
        p: &'a mut RecordParser,
    ) -> impl Iterator<Item = (u64, u64)> + 'a {
        let end = first.saturating_add(count).min(self.indexed_rows());
        let start = if first < end {
            self.offset_of(first, src, p)
        } else {
            None
        };
        RunIter {
            bytes: src.bytes(),
            parser: p,
            pos: start,
            row: first,
            end,
        }
    }

    /// Memory used by the index: the allocated checkpoint chunks plus the
    /// struct itself. Counted against the UI's 128 MiB share (§10.4).
    pub fn memory_bytes(&self) -> u64 {
        let chunks = self.chunks.iter().filter(|c| c.get().is_some()).count();
        (chunks * CHUNK_LEN * std::mem::size_of::<AtomicU64>() + std::mem::size_of::<Self>()) as u64
    }
}

/// Skips `skip` records from `offset`, then any blank or comment lines.
/// Returns the start of the record reached, or `None` at EOF.
pub(crate) fn seek(bytes: &[u8], p: &mut RecordParser, offset: u64, skip: u64) -> Option<u64> {
    let next = match p.skip(bytes, offset, skip) {
        SkipOutcome::Skipped { next } => next,
        SkipOutcome::Eof { .. } => return None,
    };
    let start = p.skip_ignorable(bytes, next as usize);
    (start < bytes.len()).then_some(start as u64)
}

struct RunIter<'a> {
    bytes: &'a [u8],
    parser: &'a mut RecordParser,
    pos: Option<u64>,
    row: u64,
    end: u64,
}

impl Iterator for RunIter<'_> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        if self.row >= self.end {
            return None;
        }
        let pos = self.pos?;
        let (start, next, _) = self.parser.next_record(self.bytes, pos)?;
        let item = (self.row, start);
        self.row += 1;
        self.pos = Some(next);
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, sync::Arc};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{
        dialect::Dialect,
        parse::{ParseOutcome, RecordRanges},
    };

    fn source(content: &[u8], dialect: Dialect) -> (tempfile::NamedTempFile, Source) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let s = Source::open(f.path(), None).unwrap().with_dialect(dialect);
        (f, s)
    }

    /// Record start offsets by sequential parsing.
    fn reference_starts(src: &Source) -> Vec<u64> {
        let mut p = RecordParser::new(src.dialect());
        let mut rec = RecordRanges::default();
        let mut pos = src.data_start();
        let mut starts = Vec::new();
        loop {
            match p.parse_at(src.bytes(), pos, &mut rec) {
                ParseOutcome::Eof => break,
                ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => {
                    starts.push(rec.start);
                    pos = next;
                }
            }
        }
        starts
    }

    /// Fills an index from the reference starts, as the indexer would.
    fn index_from(src: &Source, starts: &[u64]) -> RowIndex {
        let index = RowIndex::for_source(src);
        let cps: Vec<u64> = starts
            .iter()
            .step_by(INDEX_STRIDE as usize)
            .skip(1)
            .copied()
            .collect();
        index.push_checkpoints(&cps);
        index.finish(starts.len() as u64, src.len());
        index
    }

    fn generated(rows: usize, trailing_newline: bool) -> Vec<u8> {
        let mut out = b"id,name,note\n".to_vec();
        for i in 0..rows {
            match i % 7 {
                0 => out.extend_from_slice(format!("{i},\"multi\nline {i}\",x\n").as_bytes()),
                3 => out.extend_from_slice(b"\n"), // blank line, not a row
                _ => out.extend_from_slice(format!("{i},name{i},\"q\"\"{i}\"\n").as_bytes()),
            }
        }
        if !trailing_newline {
            out.extend_from_slice(b"last,row,end");
        }
        out
    }

    #[test]
    fn offset_of_matches_sequential_parsing() {
        let (_f, src) = source(&generated(10_000, true), Dialect::default());
        let starts = reference_starts(&src);
        assert!(starts.len() > 8_000);
        let index = index_from(&src, &starts);
        let mut p = RecordParser::new(src.dialect());
        for (r, &want) in starts.iter().enumerate() {
            assert_eq!(
                index.offset_of(r as u64, &src, &mut p),
                Some(want),
                "row {r}"
            );
        }
        let n = starts.len() as u64;
        assert_eq!(index.offset_of(n, &src, &mut p), None);
        assert_eq!(index.total_rows(), Some(n));
    }

    #[test]
    fn edge_rows() {
        let (_f, src) = source(&generated(3_000, false), Dialect::default());
        let starts = reference_starts(&src);
        let index = index_from(&src, &starts);
        let mut p = RecordParser::new(src.dialect());
        let last = starts.len() as u64 - 1;
        for r in [0, 1, 1023, 1024, 1025, 2047, 2048, last] {
            assert_eq!(
                index.offset_of(r, &src, &mut p),
                Some(starts[r as usize]),
                "row {r}"
            );
        }
        assert_eq!(
            &src.bytes()[starts[last as usize] as usize..],
            b"last,row,end"
        );
        let run: Vec<(u64, u64)> = index.offsets_for_run(1020, 10, &src, &mut p).collect();
        let want: Vec<(u64, u64)> = (1020..1030).map(|r| (r, starts[r as usize])).collect();
        assert_eq!(run, want);
        // A run past the end stops at the last row.
        assert_eq!(index.offsets_for_run(last, 10, &src, &mut p).count(), 1);
        assert_eq!(index.offsets_for_run(last + 1, 10, &src, &mut p).count(), 0);
    }

    #[test]
    fn header_only_file_has_no_rows() {
        let (_f, src) = source(b"a,b,c\n", Dialect::default());
        let starts = reference_starts(&src);
        assert!(starts.is_empty());
        let index = index_from(&src, &starts);
        let mut p = RecordParser::new(src.dialect());
        assert_eq!(index.total_rows(), Some(0));
        assert_eq!(index.offset_of(0, &src, &mut p), None);
        assert_eq!(index.locate(0), None);
        assert_eq!(index.locate_speculative(0), None);
    }

    #[test]
    fn locate_bounds() {
        let index = RowIndex::new(10);
        assert_eq!(index.locate(0), None);
        assert_eq!(index.locate_speculative(0), Some((10, 0)));
        assert_eq!(index.locate_speculative(199), Some((10, 199)));
        assert_eq!(index.locate_speculative(200), None);
        index.push_checkpoints(&[1000, 2000]);
        index.set_indexed_rows(2100);
        assert_eq!(index.locate(2099), Some((2000, 51)));
        assert_eq!(index.locate(2100), None);
        assert_eq!(index.locate_speculative(2100), Some((2000, 52)));
        assert_eq!(index.locate_speculative(2299), Some((2000, 251)));
        assert_eq!(index.locate_speculative(2300), None);
        index.truncate_to(1500);
        assert_eq!(index.published_checkpoints(), 2);
        assert_eq!(index.indexed_rows(), 1500);
        assert_eq!(index.locate(1499), Some((1000, 475)));
        index.truncate_to(0);
        assert_eq!(index.published_checkpoints(), 1);
        assert_eq!(index.checkpoint(0), Some(10));
        index.finish(5, 99);
        assert_eq!(index.total_rows(), Some(5));
        assert_eq!(index.end_offset(), 99);
        assert!(index.memory_bytes() >= 65_536 * 8);
    }

    #[test]
    fn concurrent_readers_see_written_values() {
        let iterations = if cfg!(debug_assertions) { 3 } else { 100 };
        const N: u64 = 1_000_000;
        let value = |k: u64| k * 7 + 3;
        for _ in 0..iterations {
            let index = Arc::new(RowIndex::new(value(0)));
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    let index = Arc::clone(&index);
                    std::thread::spawn(move || {
                        let mut checked = 0u64;
                        loop {
                            let rows = index.indexed_rows();
                            if rows > 0 {
                                let row = (checked * 7919) % rows;
                                let (off, skip) = index.locate(row).expect("row is indexed");
                                assert_eq!(off, value(row / INDEX_STRIDE));
                                assert_eq!(skip, row % INDEX_STRIDE);
                                checked += 1;
                            }
                            if index.is_complete() {
                                break checked;
                            }
                        }
                    })
                })
                .collect();
            let mut batch = Vec::new();
            for k in 1..N {
                batch.push(value(k));
                if batch.len() == 4096 || k == N - 1 {
                    index.push_checkpoints(&batch);
                    batch.clear();
                    index.set_indexed_rows((k + 1) * INDEX_STRIDE);
                }
            }
            index.finish(N * INDEX_STRIDE, 0);
            for r in readers {
                r.join().unwrap();
            }
            assert_eq!(index.published_checkpoints(), N);
        }
    }
}
