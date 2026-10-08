//! Run generation (spec §8.4 step 2): key extraction from the parent view,
//! the in-memory record buffer, parallel slice sorting and run files.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    SortCtx, TieBreaker,
    key::{SortRecord, encode_key},
};
use crate::{
    exec::Executor,
    jobs::{CHECK_INTERVAL, JobControl, JobError, SortPhase, SortProgress},
    parse::{ParseOutcome, RecordParser, RecordRanges, SkipOutcome},
    view::View,
};

/// Write buffer of a run file (§8.4).
pub(crate) const WRITE_BUFFER: usize = 1 << 20;

/// One sorted run on disk.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) path: PathBuf,
    pub(crate) count: u64,
}

/// The record buffer, kept as slabs of at most `cap` records so a spill can
/// hand each slab to its own worker without copying.
#[derive(Debug)]
pub(crate) struct Slabs {
    slabs: Vec<Vec<SortRecord>>,
    cap: usize,
    len: usize,
}

impl Slabs {
    pub(crate) fn new(cap: usize) -> Slabs {
        Slabs {
            slabs: Vec::new(),
            cap: cap.max(1),
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn extend(&mut self, mut recs: &[SortRecord]) {
        self.len += recs.len();
        while !recs.is_empty() {
            if self.slabs.last().is_none_or(|s| s.len() == self.cap) {
                self.slabs
                    .push(Vec::with_capacity(self.cap.min(recs.len().max(1024))));
            }
            let slab = self.slabs.last_mut().expect("a slab");
            let n = (self.cap - slab.len()).min(recs.len());
            slab.extend_from_slice(&recs[..n]);
            recs = &recs[n..];
        }
    }

    pub(crate) fn take(&mut self) -> Vec<Vec<SortRecord>> {
        self.len = 0;
        std::mem::take(&mut self.slabs)
    }
}

/// Extracts the first-key records of parent positions `first .. first +
/// count`. Order inside the chunk does not matter (the comparator is total),
/// so `Ordered` parents are read in row-id order, which turns random seeks
/// into a forward walk.
pub(crate) fn extract_chunk(
    ctx: &SortCtx,
    parent: &View,
    first: u64,
    count: u64,
    ctl: &JobControl,
) -> Result<Vec<SortRecord>, JobError> {
    ctl.check()?;
    let mut ex = Extractor::new(ctx, ctl, count as usize);
    match parent {
        View::All => ex.range(first, count)?,
        View::Filtered { .. } | View::Dupes { .. } => {
            let ids = parent.row_ids(first, count as usize);
            ex.ids(&ids)?;
        }
        View::Ordered { list, .. } => {
            let mut ids = list.read(first, count as usize);
            ids.sort_unstable();
            ex.ids(&ids)?;
        }
    }
    Ok(ex.out)
}

struct Extractor<'a> {
    ctx: &'a SortCtx,
    ctl: &'a JobControl,
    parser: RecordParser,
    rec: RecordRanges,
    scratch: Vec<u8>,
    /// Edited value (`crate::edit`).
    edited: Vec<u8>,
    text: String,
    since_check: u64,
    out: Vec<SortRecord>,
}

impl<'a> Extractor<'a> {
    fn new(ctx: &'a SortCtx, ctl: &'a JobControl, capacity: usize) -> Extractor<'a> {
        Extractor {
            ctx,
            ctl,
            parser: RecordParser::new(ctx.src.dialect()),
            rec: RecordRanges::default(),
            scratch: Vec::new(),
            edited: Vec::new(),
            text: String::new(),
            since_check: 0,
            out: Vec::with_capacity(capacity),
        }
    }

    /// Checks the job tokens every [`CHECK_INTERVAL`] bytes of input.
    fn tick(&mut self, bytes: u64) -> Result<(), JobError> {
        self.since_check += bytes;
        if self.since_check >= CHECK_INTERVAL as u64 {
            self.since_check = 0;
            self.ctl.check()?;
        }
        Ok(())
    }

    /// Parses the record at `offset` as row `row_id` and encodes its key.
    /// Returns the offset after it, or `None` at EOF.
    fn record(&mut self, row_id: u64, offset: u64) -> Result<Option<u64>, JobError> {
        let bytes = self.ctx.src.bytes();
        let next = match self.parser.parse_at(bytes, offset, &mut self.rec) {
            ParseOutcome::Record { next } | ParseOutcome::UnterminatedQuote { next } => next,
            ParseOutcome::Eof => return Ok(None),
        };
        let spec = &self.ctx.keys[0];
        let v = (spec.field < self.rec.fields.len()).then(|| {
            let v = self
                .parser
                .field_value(bytes, &self.rec, spec.field, &mut self.scratch);
            self.ctx.src.edits().apply(spec.field, v, &mut self.edited)
        });
        let prefix = encode_key(spec, v, &self.ctx.nulls, self.ctx.enc, &mut self.text);
        self.out.push(SortRecord { prefix, row_id });
        self.tick(next.saturating_sub(offset))?;
        Ok(Some(next))
    }

    /// Rows `first .. first + count`, walked forward from one seek.
    fn range(&mut self, first: u64, count: u64) -> Result<(), JobError> {
        let Some(mut pos) = self
            .ctx
            .index
            .offset_of(first, &self.ctx.src, &mut self.parser)
        else {
            return Ok(());
        };
        for row in first..first + count {
            match self.record(row, pos)? {
                Some(next) => pos = next,
                None => break,
            }
        }
        Ok(())
    }

    /// The rows `ids` (ascending): skips forward when the next id is close,
    /// seeks through the index otherwise.
    fn ids(&mut self, ids: &[u64]) -> Result<(), JobError> {
        let near = 2 * self.ctx.index.stride();
        let bytes = self.ctx.src.bytes();
        let mut cursor: Option<(u64, u64)> = None;
        for &id in ids {
            let offset = match cursor {
                Some((row, off)) if id >= row && id - row <= near => {
                    if id == row {
                        Some(off)
                    } else {
                        match self.parser.skip(bytes, off, id - row) {
                            SkipOutcome::Skipped { next } => {
                                self.tick(next - off)?;
                                Some(next)
                            }
                            SkipOutcome::Eof { .. } => None,
                        }
                    }
                }
                _ => self
                    .ctx
                    .index
                    .offset_of(id, &self.ctx.src, &mut self.parser),
            };
            let Some(offset) = offset else {
                cursor = None;
                continue;
            };
            cursor = self.record(id, offset)?.map(|next| (id + 1, next));
        }
        Ok(())
    }
}

/// Sorts `slab` with a worker-local comparator. Cancellation can't interrupt
/// the sort itself, so slabs are bounded (`SortOptions::max_slice_records`).
fn sort_slab(ctx: &Arc<SortCtx>, slab: &mut [SortRecord]) {
    let mut tb = TieBreaker::new(Arc::clone(ctx));
    slab.sort_unstable_by(|a, b| tb.cmp(a, b));
}

/// Writes a sorted slab as a run file.
fn write_run(path: &Path, slab: &[SortRecord]) -> Result<Run, JobError> {
    let ctx = || format!("writing run file {}", path.display());
    let file = File::create(path).map_err(|e| JobError::io(ctx(), e))?;
    let mut w = BufWriter::with_capacity(WRITE_BUFFER, file);
    for r in slab {
        w.write_all(&r.to_bytes())
            .map_err(|e| JobError::io(ctx(), e))?;
    }
    w.flush().map_err(|e| JobError::io(ctx(), e))?;
    Ok(Run {
        path: path.to_path_buf(),
        count: slab.len() as u64,
    })
}

/// Awaits every task (so none is still writing when the job returns) and
/// returns the results in order, or the first error.
async fn join_all<T: Send + 'static>(
    tasks: Vec<tokio::task::JoinHandle<Result<T, JobError>>>,
) -> Result<Vec<T>, JobError> {
    let mut out = Vec::with_capacity(tasks.len());
    let mut failure = None;
    for t in tasks {
        match t.await {
            Ok(Ok(v)) => out.push(v),
            Ok(Err(e)) => {
                failure.get_or_insert(e);
            }
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => {
                failure.get_or_insert(JobError::Cancelled);
            }
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// Sorts every slab in its own `exec.run` and writes each as a run file
/// (`run-00042.bin`) in `dir`. Run numbers start at `first_run`.
pub(crate) async fn spill(
    ctx: &Arc<SortCtx>,
    exec: &Executor,
    ctl: &JobControl,
    progress: &Arc<SortProgress>,
    slabs: Vec<Vec<SortRecord>>,
    dir: &Path,
    first_run: usize,
) -> Result<Vec<Run>, JobError> {
    progress.start_phase(SortPhase::SortRuns, slabs.len() as u64);
    let tasks = slabs
        .into_iter()
        .enumerate()
        .map(|(i, mut slab)| {
            let (ctx, ctl, exec, progress) = (
                Arc::clone(ctx),
                ctl.clone(),
                exec.clone(),
                Arc::clone(progress),
            );
            let path = dir.join(format!("run-{:05}.bin", first_run + i));
            tokio::spawn(async move {
                exec.run(move || {
                    ctl.check()?;
                    sort_slab(&ctx, &mut slab);
                    ctl.check()?;
                    let run = write_run(&path, &slab)?;
                    progress.add(1);
                    Ok(run)
                })
                .await
            })
        })
        .collect();
    join_all(tasks).await
}

/// Sorts every slab in its own `exec.run` (the in-memory path).
pub(crate) async fn sort_slabs(
    ctx: &Arc<SortCtx>,
    exec: &Executor,
    ctl: &JobControl,
    progress: &Arc<SortProgress>,
    slabs: Vec<Vec<SortRecord>>,
) -> Result<Vec<Vec<SortRecord>>, JobError> {
    let tasks = slabs
        .into_iter()
        .map(|mut slab| {
            let (ctx, ctl, exec, progress) = (
                Arc::clone(ctx),
                ctl.clone(),
                exec.clone(),
                Arc::clone(progress),
            );
            tokio::spawn(async move {
                exec.run(move || {
                    ctl.check()?;
                    sort_slab(&ctx, &mut slab);
                    progress.add(1);
                    Ok(slab)
                })
                .await
            })
        })
        .collect();
    join_all(tasks).await
}
