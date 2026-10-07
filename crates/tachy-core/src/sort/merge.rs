//! K-way merge (spec §8.4 step 3): a binary min-heap over run readers, each
//! with a read buffer of up to 1 MiB. More runs than the fan-in → several
//! passes through intermediate runs.
//!
//! The heap is hand-written because the comparator is stateful (it reads
//! full values from the mmap on ties), which `std::collections::BinaryHeap`
//! can't express.

use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
    sync::Arc,
};

use super::{
    SortCtx, TieBreaker,
    key::{RECORD_BYTES, SortRecord},
    runs::{Run, WRITE_BUFFER},
};
use crate::{
    exec::Executor,
    jobs::{JobControl, JobError, SortProgress},
    view::RowIdWriter,
};

/// Read buffer per run (§8.4).
pub(crate) const READ_BUFFER: usize = 1 << 20;

/// Output records between checks of the job tokens (~1 MiB of runs).
const CHECK_RECORDS: u64 = (1 << 20) / RECORD_BYTES as u64;

/// A sorted source of records: an in-memory slab or a run file.
pub(crate) enum RunReader {
    Memory {
        recs: Vec<SortRecord>,
        pos: usize,
    },
    File {
        r: BufReader<File>,
        left: u64,
        path: std::path::PathBuf,
    },
}

impl RunReader {
    pub(crate) fn memory(recs: Vec<SortRecord>) -> RunReader {
        RunReader::Memory { recs, pos: 0 }
    }

    fn open(run: &Run, buf: usize) -> Result<RunReader, JobError> {
        let f = File::open(&run.path)
            .map_err(|e| JobError::io(format!("reading run file {}", run.path.display()), e))?;
        Ok(RunReader::File {
            r: BufReader::with_capacity(buf, f),
            left: run.count,
            path: run.path.clone(),
        })
    }

    fn next(&mut self) -> Result<Option<SortRecord>, JobError> {
        match self {
            RunReader::Memory { recs, pos } => {
                let r = recs.get(*pos).copied();
                *pos += 1;
                Ok(r)
            }
            RunReader::File { r, left, path } => {
                if *left == 0 {
                    return Ok(None);
                }
                let mut b = [0u8; RECORD_BYTES];
                r.read_exact(&mut b)
                    .map_err(|e| JobError::io(format!("reading run file {}", path.display()), e))?;
                *left -= 1;
                Ok(Some(SortRecord::from_bytes(&b)))
            }
        }
    }
}

/// Min-heap of `(record, source)`.
struct Heap {
    items: Vec<(SortRecord, usize)>,
}

impl Heap {
    fn sift_down(&mut self, mut i: usize, tb: &mut TieBreaker) {
        let n = self.items.len();
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut min = i;
            if l < n && tb.cmp(&self.items[l].0, &self.items[min].0).is_lt() {
                min = l;
            }
            if r < n && tb.cmp(&self.items[r].0, &self.items[min].0).is_lt() {
                min = r;
            }
            if min == i {
                return;
            }
            self.items.swap(i, min);
            i = min;
        }
    }

    fn build(&mut self, tb: &mut TieBreaker) {
        for i in (0..self.items.len() / 2).rev() {
            self.sift_down(i, tb);
        }
    }
}

/// Merges `sources` in order, calling `emit` for each record. Returns the
/// number of records.
fn merge_with(
    ctx: &Arc<SortCtx>,
    mut sources: Vec<RunReader>,
    ctl: &JobControl,
    progress: &SortProgress,
    mut emit: impl FnMut(&SortRecord) -> io::Result<()>,
    err_ctx: &str,
) -> Result<u64, JobError> {
    let mut tb = TieBreaker::new(Arc::clone(ctx));
    let mut heap = Heap {
        items: Vec::with_capacity(sources.len()),
    };
    for (i, s) in sources.iter_mut().enumerate() {
        if let Some(r) = s.next()? {
            heap.items.push((r, i));
        }
    }
    heap.build(&mut tb);
    let mut n = 0u64;
    while let Some(&(rec, src)) = heap.items.first() {
        emit(&rec).map_err(|e| JobError::io(err_ctx, e))?;
        n += 1;
        if n.is_multiple_of(CHECK_RECORDS) {
            progress.add(CHECK_RECORDS);
            ctl.check()?;
        }
        match sources[src].next()? {
            Some(next) => heap.items[0] = (next, src),
            None => {
                let last = heap.items.len() - 1;
                heap.items.swap(0, last);
                heap.items.pop();
            }
        }
        heap.sift_down(0, &mut tb);
    }
    progress.add(n % CHECK_RECORDS);
    Ok(n)
}

/// Merges `sources` into the permutation file (row ids only).
pub(crate) fn merge_into_ids(
    ctx: &Arc<SortCtx>,
    sources: Vec<RunReader>,
    writer: &mut RowIdWriter,
    ctl: &JobControl,
    progress: &SortProgress,
    err_ctx: &str,
) -> Result<u64, JobError> {
    merge_with(
        ctx,
        sources,
        ctl,
        progress,
        |r| writer.push(r.row_id),
        err_ctx,
    )
}

/// Merges runs into one run file at `path`.
fn merge_into_run(
    ctx: &Arc<SortCtx>,
    sources: Vec<RunReader>,
    path: &Path,
    ctl: &JobControl,
    progress: &SortProgress,
) -> Result<Run, JobError> {
    let err_ctx = format!("writing run file {}", path.display());
    let f = File::create(path).map_err(|e| JobError::io(&err_ctx, e))?;
    let mut w = BufWriter::with_capacity(WRITE_BUFFER, f);
    let count = merge_with(
        ctx,
        sources,
        ctl,
        progress,
        |r| w.write_all(&r.to_bytes()),
        &err_ctx,
    )?;
    w.flush().map_err(|e| JobError::io(&err_ctx, e))?;
    Ok(Run {
        path: path.to_path_buf(),
        count,
    })
}

/// Number of merge passes for `runs` runs with `fan_in` (≥ 2): intermediate
/// passes while more than `fan_in` runs remain, then the final pass.
pub fn merge_passes(runs: usize, fan_in: usize) -> u32 {
    let fan_in = fan_in.max(2);
    let mut n = runs;
    let mut passes = 1;
    while n > fan_in {
        n = n.div_ceil(fan_in);
        passes += 1;
    }
    passes
}

/// Merges `runs` (in several passes when there are more than `fan_in`) into
/// `writer`. Intermediate runs go to `dir` and are deleted once merged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn merge_runs(
    ctx: &Arc<SortCtx>,
    exec: &Executor,
    ctl: &JobControl,
    progress: &Arc<SortProgress>,
    mut runs: Vec<Run>,
    fan_in: usize,
    read_buf: usize,
    dir: &Path,
    mut writer: RowIdWriter,
    total: u64,
    sink_ctx: String,
) -> Result<RowIdWriter, JobError> {
    let passes = merge_passes(runs.len(), fan_in);
    for pass in 1..passes {
        progress.start_merge_pass(pass, passes, total);
        let mut next = Vec::with_capacity(runs.len().div_ceil(fan_in));
        let mut it = runs.into_iter().peekable();
        let mut group_no = 0;
        while it.peek().is_some() {
            let group: Vec<Run> = it.by_ref().take(fan_in).collect();
            if group.len() == 1 {
                // Nothing to merge: carried to the next pass as is.
                progress.add(group[0].count);
                next.extend(group);
                continue;
            }
            let path = dir.join(format!("merge-{pass}-{group_no:05}.bin"));
            group_no += 1;
            let (ctx, ctl, progress) = (Arc::clone(ctx), ctl.clone(), Arc::clone(progress));
            let run = exec
                .run(move || {
                    let sources = group
                        .iter()
                        .map(|r| RunReader::open(r, read_buf))
                        .collect::<Result<Vec<_>, _>>()?;
                    let run = merge_into_run(&ctx, sources, &path, &ctl, &progress)?;
                    for r in &group {
                        let _ = std::fs::remove_file(&r.path);
                    }
                    Ok::<_, JobError>(run)
                })
                .await?;
            next.push(run);
        }
        runs = next;
    }
    progress.start_merge_pass(passes, passes, total);
    let (ctx, ctl, progress) = (Arc::clone(ctx), ctl.clone(), Arc::clone(progress));
    exec.run(move || {
        let sources = runs
            .iter()
            .map(|r| RunReader::open(r, read_buf))
            .collect::<Result<Vec<_>, _>>()?;
        merge_into_ids(&ctx, sources, &mut writer, &ctl, &progress, &sink_ctx)?;
        for r in &runs {
            let _ = std::fs::remove_file(&r.path);
        }
        Ok(writer)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes() {
        assert_eq!(merge_passes(1, 256), 1);
        assert_eq!(merge_passes(256, 256), 1);
        assert_eq!(merge_passes(257, 256), 2);
        assert_eq!(merge_passes(9, 3), 2);
        assert_eq!(merge_passes(10, 3), 3);
        assert_eq!(merge_passes(10, 1), 4);
    }
}
