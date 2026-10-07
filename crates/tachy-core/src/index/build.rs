//! Orchestration of the parallel indexer: chunk planning, dispatch through the
//! [`Executor`], in-order stitching and progressive publication (spec §5.3,
//! README §A2).
//!
//! `build_index` is an `async fn` that the UI starts with `tokio::spawn` when
//! a tab opens. It splits `[data_start, len)` into chunks (each ending just
//! after a `\n`, see [`super::scan`]), runs a batch of chunk scans
//! concurrently through the executor, stitches the batch in file order,
//! publishes its checkpoints and row count, then starts the next batch. Rows
//! become navigable from the start of the file while indexing continues, and
//! only one batch of scan results is alive at a time.

use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{
    RowIndex,
    scan::{self, Cancelled, ChunkScan, ChunkState, ScanConfig},
    warnings,
};
use crate::{dialect::SniffReport, exec::Executor, source::Source};

/// Smallest chunk the memory cap shrinks chunks to.
const MIN_CHUNK: u64 = 1 << 20;

/// Tuning of [`build_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOptions {
    /// Nominal chunk size in bytes (default 64 MiB; tests go down to 1).
    /// Each chunk is extended to the next `\n`.
    pub chunk_size: u64,
    /// Chunks per batch; 0 means `threads × 2`.
    pub batch: usize,
    /// The memory budget (`--mem`, M5-01). Scan results of a batch may use at
    /// most a quarter of it, in the worst case of 2-byte records.
    pub memory_budget: u64,
    /// Count ragged rows (§6.4). On by default.
    pub count_ragged: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            chunk_size: 64 << 20,
            batch: 0,
            memory_budget: 2 << 30,
            count_ragged: true,
        }
    }
}

/// Which strategy indexed the file (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPath {
    /// One run per chunk, and every chunk ended outside quotes.
    Fast,
    /// Speculative two-state runs from the start (the sniffer saw quoted
    /// newlines).
    Slow,
    /// The fast path found a quoted newline across a chunk boundary and
    /// re-indexed from `from_chunk` with the slow path.
    FastThenSlow {
        /// Index of the first chunk scanned with the slow path.
        from_chunk: u64,
    },
}

/// Result of a finished [`build_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSummary {
    /// Exact number of rows (records after the header).
    pub total_rows: u64,
    /// Rows whose field count differs from the column count.
    pub ragged_rows: u64,
    /// The file ends inside a quoted field (§16).
    pub unterminated_quote: bool,
    /// The strategy used.
    pub path_used: IndexPath,
}

/// Why [`build_index`] stopped.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum IndexError {
    /// The token was cancelled (tab closed, dialect changed, reload). The
    /// `RowIndex` is incomplete and must not be reused.
    #[error("indexing cancelled")]
    Cancelled,
    /// A chunk task panicked.
    #[error("indexing failed: {0}")]
    Failed(String),
}

impl From<Cancelled> for IndexError {
    fn from(_: Cancelled) -> Self {
        IndexError::Cancelled
    }
}

/// Chunk size and batch size after the memory cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    pub chunk_size: u64,
    pub batch: usize,
}

impl Plan {
    /// Caps `batch × worst-case starts × 4 B × runs` at a quarter of the
    /// memory budget: shrinks the chunk size first (down to 1 MiB, keeping
    /// every permit busy), then the batch.
    pub(crate) fn new(opts: &IndexOptions, threads: usize, slow: bool) -> Plan {
        let runs: u64 = if slow { 2 } else { 1 };
        let mut batch = if opts.batch == 0 {
            threads.max(1) * 2
        } else {
            opts.batch
        };
        let mut chunk_size = opts.chunk_size.max(1);
        let quarter = (opts.memory_budget / 4).max(1);
        let worst = |chunk: u64| (chunk / 2 + 1) * 4 * runs;
        if batch as u64 * worst(chunk_size) > quarter {
            let fit = (quarter / (batch as u64 * runs * 4)).saturating_sub(1) * 2;
            chunk_size = chunk_size.min(fit.max(MIN_CHUNK));
            if batch as u64 * worst(chunk_size) > quarter {
                batch = usize::try_from(quarter / worst(chunk_size))
                    .unwrap_or(usize::MAX)
                    .max(1);
            }
        }
        Plan { chunk_size, batch }
    }
}

/// End of the chunk whose nominal end is `nominal`: just after the first
/// `\n` at or after `nominal - 1`, or EOF.
fn align(bytes: &[u8], nominal: usize, cancel: &CancellationToken) -> Result<usize, Cancelled> {
    let len = bytes.len();
    if nominal >= len {
        return Ok(len);
    }
    if bytes[nominal - 1] == b'\n' {
        return Ok(nominal);
    }
    let mut from = nominal;
    while from < len {
        let to = (from + (1 << 20)).min(len);
        if let Some(i) = memchr::memchr(b'\n', &bytes[from..to]) {
            return Ok(from + i + 1);
        }
        if cancel.is_cancelled() {
            return Err(Cancelled);
        }
        from = to;
    }
    Ok(len)
}

/// Up to `batch` chunks starting at `pos`.
pub(crate) fn plan_chunks(
    bytes: &[u8],
    pos: u64,
    plan: Plan,
    cancel: &CancellationToken,
) -> Result<Vec<(u64, u64)>, Cancelled> {
    let len = bytes.len() as u64;
    let mut out = Vec::with_capacity(plan.batch);
    let mut s = pos;
    while out.len() < plan.batch && s < len {
        let nominal = s.saturating_add(plan.chunk_size).min(len);
        let e = align(bytes, nominal as usize, cancel)? as u64;
        out.push((s, e));
        s = e;
    }
    Ok(out)
}

/// Stitches chunk scans in file order: assigns row numbers, keeps every
/// 1,024th record start as a checkpoint, joins records cut by chunk
/// boundaries for ragged counting.
#[derive(Debug, Clone)]
pub struct Stitcher {
    rows: u64,
    prev_end: ChunkState,
    /// Delimiters so far of a record still open at the last chunk's end.
    open: Option<u64>,
    ragged: u64,
    width: Option<u64>,
    stride: u64,
    checkpoints: Vec<u64>,
}

impl Stitcher {
    /// A stitcher at the start of the data, keeping one checkpoint every
    /// `stride` rows and counting ragged rows against `width` if given.
    pub fn new(width: Option<u64>, stride: u64) -> Self {
        Stitcher {
            rows: 0,
            prev_end: ChunkState::Outside,
            open: None,
            ragged: 0,
            width,
            stride: stride.max(1),
            checkpoints: Vec::new(),
        }
    }

    /// The start state the next chunk must have been scanned with.
    pub fn expects(&self) -> ChunkState {
        self.prev_end
    }

    /// Rows so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Ragged rows so far (complete records only).
    pub fn ragged(&self) -> u64 {
        self.ragged
    }

    fn close(&mut self, delims: u64) {
        if let Some(w) = self.width
            && delims + 1 != w
        {
            self.ragged += 1;
        }
    }

    /// Appends the next chunk.
    ///
    /// # Panics
    ///
    /// When `c.start_state` is not [`Stitcher::expects`].
    pub fn stitch(&mut self, c: &ChunkScan) {
        assert_eq!(
            c.start_state, self.prev_end,
            "chunk scanned with the wrong start state"
        );
        if let Some(open) = self.open.take() {
            let head = c.head.expect("an Inside chunk has a head");
            let total = open + head.delims;
            if head.closed {
                self.close(total);
            } else {
                self.open = Some(total);
            }
        }
        for rel in c.starts.iter() {
            if self.rows.is_multiple_of(self.stride) && self.rows > 0 {
                self.checkpoints.push(c.start + rel);
            }
            self.rows += 1;
        }
        if self.width.is_some() {
            self.ragged += c.ragged;
        }
        if let Some(t) = c.tail_delims {
            self.open = Some(t);
        }
        self.prev_end = c.end_state;
    }

    /// Checkpoints found since the last call.
    pub fn take_checkpoints(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.checkpoints)
    }

    /// Ends the file: closes a record left open (unterminated quote).
    /// Returns `(rows, ragged, unterminated)`.
    pub fn finish(&mut self) -> (u64, u64, bool) {
        let unterminated = self.prev_end == ChunkState::Inside;
        if let Some(open) = self.open.take() {
            self.close(open);
        }
        (self.rows, self.ragged, unterminated)
    }
}

/// Builds the sparse index of `src` into `index` (spec §5.3).
///
/// - Holds a `ScanGuard` while it runs (§5.1).
/// - Uses the slow path from the start when `report.quoted_newlines`, the
///   fast path otherwise (switching to the slow path at the first chunk
///   boundary that lies inside quotes).
/// - Publishes checkpoints and `indexed_rows` after each batch, and
///   `bytes_scanned` continuously.
/// - On cancellation, every scanner stops within 64 KiB and this returns
///   [`IndexError::Cancelled`], leaving `index` incomplete.
pub async fn build_index(
    src: Arc<Source>,
    index: Arc<RowIndex>,
    report: SniffReport,
    exec: Executor,
    cancel: CancellationToken,
    opts: IndexOptions,
) -> Result<IndexSummary, IndexError> {
    let _guard = src.begin_scan();
    let len = src.len();
    let data_start = src.data_start();

    let width = if opts.count_ragged {
        // The width may parse the first record: keep it off the async task.
        let src = Arc::clone(&src);
        Some(exec.run(move || src.width() as u64).await)
    } else {
        None
    };
    let cfg = ScanConfig::new(src.dialect(), width);

    // Without a quote byte there is no quoted state: the fast path is exact.
    let mut slow = report.quoted_newlines && cfg.quote.is_some();
    let mut path = if slow {
        IndexPath::Slow
    } else {
        IndexPath::Fast
    };
    let mut stitcher = Stitcher::new(width, index.stride());
    let mut pos = data_start;
    let mut chunk_no = 0u64;

    while pos < len {
        if cancel.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        if !slow && stitcher.expects() == ChunkState::Inside {
            // The last batch ended inside quotes: switch to the slow path.
            slow = true;
            path = IndexPath::FastThenSlow {
                from_chunk: chunk_no,
            };
        }
        let plan = Plan::new(&opts, exec.threads(), slow);
        let bounds = {
            let src = Arc::clone(&src);
            let cancel = cancel.clone();
            exec.run(move || plan_chunks(src.bytes(), pos, plan, &cancel))
                .await?
        };

        // Dispatch every run of the batch.
        let mut handles = Vec::new();
        for (k, &(s, e)) in bounds.iter().enumerate() {
            let states: &[ChunkState] = if k == 0 {
                // The state at the batch start is known.
                &[stitcher.expects()][..]
            } else if slow {
                &[ChunkState::Outside, ChunkState::Inside][..]
            } else {
                &[ChunkState::Outside][..]
            };
            for (j, &state) in states.iter().enumerate() {
                let (src, index, cancel, exec) = (
                    Arc::clone(&src),
                    Arc::clone(&index),
                    cancel.clone(),
                    exec.clone(),
                );
                let report_progress = j == 0;
                let task = tokio::spawn(async move {
                    exec.run(move || {
                        let progress = report_progress.then_some(&index.bytes_scanned);
                        scan::scan_chunk(src.bytes(), s, e, state, &cfg, &cancel, progress)
                    })
                    .await
                });
                handles.push((k, task));
            }
        }

        // Wait for all of them, even after a failure, so no scanner outlives
        // this call.
        let mut results: Vec<Vec<ChunkScan>> = vec![Vec::new(); bounds.len()];
        let mut failure = None;
        for (k, task) in handles {
            match task.await {
                Ok(Ok(scan)) => results[k].push(scan),
                Ok(Err(Cancelled)) => {
                    failure.get_or_insert(IndexError::Cancelled);
                }
                Err(e) => failure = Some(IndexError::Failed(e.to_string())),
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        if cancel.is_cancelled() {
            return Err(IndexError::Cancelled);
        }

        // Stitch in order.
        for runs in &results {
            let want = stitcher.expects();
            match runs.iter().find(|r| r.start_state == want) {
                Some(run) => {
                    stitcher.stitch(run);
                    pos = run.end;
                    chunk_no += 1;
                }
                None => {
                    // Fast path: the previous chunk ended inside quotes.
                    // Discard the rest of the batch and re-index from here
                    // with the slow path.
                    debug_assert!(!slow);
                    slow = true;
                    path = IndexPath::FastThenSlow {
                        from_chunk: chunk_no,
                    };
                    index.set_bytes_scanned(pos - data_start);
                    break;
                }
            }
        }
        drop(results);
        index.push_checkpoints(&stitcher.take_checkpoints());
        index.set_ragged_rows(stitcher.ragged());
        index.set_indexed_rows(stitcher.rows());
    }

    let (total_rows, ragged_rows, unterminated_quote) = stitcher.finish();
    if unterminated_quote {
        index.set_warning(warnings::UNTERMINATED_QUOTE);
    }
    index.set_ragged_rows(ragged_rows);
    index.finish(total_rows, end_offset(src.bytes()));
    tracing::debug!(
        file = src.display_name(),
        rows = total_rows,
        ragged = ragged_rows,
        ?path,
        index_bytes = index.memory_bytes(),
        "index built"
    );
    Ok(IndexSummary {
        total_rows,
        ragged_rows,
        unterminated_quote,
        path_used: path,
    })
}

/// The file length minus a trailing `\n` or `\r\n`.
fn end_offset(bytes: &[u8]) -> u64 {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    end as u64
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::index::scan::{Head, Starts};

    fn chunk(
        start: u64,
        start_state: ChunkState,
        end_state: ChunkState,
        starts: &[u32],
        head: Option<Head>,
        tail: Option<u64>,
        ragged: u64,
    ) -> ChunkScan {
        ChunkScan {
            start,
            end: start + 100,
            start_state,
            end_state,
            starts: Starts::Narrow(starts.to_vec()),
            head,
            tail_delims: tail,
            ragged,
        }
    }

    use ChunkState::{Inside, Outside};

    #[test]
    fn stitch_all_state_combinations() {
        let mut s = Stitcher::new(Some(3), 1024);
        // Outside → Outside: two records, one ragged.
        s.stitch(&chunk(0, Outside, Outside, &[0, 10], None, None, 1));
        assert_eq!((s.rows(), s.ragged(), s.expects()), (2, 1, Outside));
        // Outside → Inside: one record, open with 1 delimiter.
        s.stitch(&chunk(100, Outside, Inside, &[5], None, Some(1), 0));
        assert_eq!(s.expects(), Inside);
        // Inside → Inside: no record start, 1 more delimiter, still open.
        s.stitch(&chunk(
            200,
            Inside,
            Inside,
            &[],
            Some(Head {
                delims: 1,
                closed: false,
            }),
            None,
            0,
        ));
        assert_eq!((s.rows(), s.ragged()), (3, 1));
        // Inside → Outside: the open record closes with 2 delimiters
        // (exact), plus one new record.
        s.stitch(&chunk(
            300,
            Inside,
            Outside,
            &[50],
            Some(Head {
                delims: 0,
                closed: true,
            }),
            None,
            0,
        ));
        assert_eq!((s.rows(), s.ragged(), s.expects()), (4, 1, Outside));
        // Inside → Inside with a closed head and a new open tail, then EOF.
        s.stitch(&chunk(400, Outside, Inside, &[0], None, Some(0), 0));
        s.stitch(&chunk(
            500,
            Inside,
            Inside,
            &[20],
            Some(Head {
                delims: 5,
                closed: true,
            }),
            Some(2),
            0,
        ));
        // Head closed with 5 delimiters: ragged. Tail open with 2.
        assert_eq!((s.rows(), s.ragged()), (6, 2));
        assert_eq!(s.finish(), (6, 2, true));
    }

    #[test]
    fn checkpoints_every_stride() {
        let mut s = Stitcher::new(None, 1024);
        let starts: Vec<u32> = (0..3000).collect();
        s.stitch(&chunk(1000, Outside, Outside, &starts, None, None, 0));
        assert_eq!(s.take_checkpoints(), [1000 + 1024, 1000 + 2048]);
        assert_eq!(s.finish(), (3000, 0, false));
    }

    #[test]
    #[should_panic(expected = "wrong start state")]
    fn stitch_rejects_mismatched_state() {
        let mut s = Stitcher::new(None, 1024);
        s.stitch(&chunk(0, Inside, Outside, &[], None, None, 0));
    }

    #[test]
    fn plan_caps_memory() {
        let opts = IndexOptions::default();
        // 8 threads, fast: 16 chunks × 64 MiB would be 2 GiB of starts.
        let p = Plan::new(&opts, 8, false);
        assert_eq!(p.batch, 16);
        assert!(16 * (p.chunk_size / 2 + 1) * 4 <= 512 << 20);
        let p = Plan::new(&opts, 8, true);
        assert!(p.batch as u64 * (p.chunk_size / 2 + 1) * 8 <= 512 << 20);
        // A tiny budget reduces the batch too.
        let tiny = IndexOptions {
            memory_budget: 8 << 20,
            ..opts
        };
        let p = Plan::new(&tiny, 8, false);
        assert_eq!(p.chunk_size, MIN_CHUNK);
        assert_eq!(p.batch, 1);
        // Small chunks are left alone.
        let small = IndexOptions {
            chunk_size: 7,
            batch: 3,
            ..opts
        };
        assert_eq!(
            Plan::new(&small, 8, true),
            Plan {
                chunk_size: 7,
                batch: 3
            }
        );
    }

    #[test]
    fn chunks_end_after_newlines() {
        let bytes = b"aaaa\nbb\n\nccccccc\nd";
        let plan = Plan {
            chunk_size: 3,
            batch: 10,
        };
        let c = plan_chunks(bytes, 0, plan, &CancellationToken::new()).unwrap();
        assert_eq!(c, [(0, 5), (5, 8), (8, 17), (17, 18)]);
    }

    #[test]
    fn end_offset_strips_one_newline() {
        assert_eq!(end_offset(b"a\r\n"), 1);
        assert_eq!(end_offset(b"a\n"), 1);
        assert_eq!(end_offset(b"a"), 1);
        assert_eq!(end_offset(b""), 0);
    }
}
