//! Core side of background jobs: cancellation and pause tokens, progress
//! counters and the memory budget (spec §4.3, §10; README §A2).
//!
//! The UI-side scheduler (`JobManager`) lives in the `tachy` crate.
//!
//! The pieces a job uses:
//!
//! - a [`CancellationToken`] (a child of its tab's token) and a [`PauseToken`];
//!   blocking workers bundle them in a [`Checkpoint`] (or the owned
//!   [`JobControl`]) and call [`Checkpoint::check`] at least every
//!   [`CHECK_INTERVAL`] bytes of input;
//! - a [`Progress`] value: shared atomic counters the UI reads when it draws,
//!   so high-frequency progress is never sent as messages;
//! - a byte budget granted by [`MemoryBudget::try_allocate`] at job start,
//!   which the job uses to size its buffers (advisory, §10.4);
//! - [`JobError`] as its error type.

use std::{
    io,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// What a background job does (§10.1). The UI colours jobs by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// Building the row index of a file (started by opening it).
    Index,
    /// Evaluating a filter over a view.
    Filter,
    /// External merge sort of a view.
    Sort,
    /// Full-column statistics (`:profile`).
    Profile,
    /// Writing a view to a file.
    Export,
    /// Finding (`dupes`) or removing (`dedupe`) duplicate rows of a view.
    Dupes,
}

/// Where a job is in its lifecycle (§10.2):
/// `Queued → Running ⇄ Paused → (Done | Failed | Cancelled)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting for a concurrency slot (§10.3).
    Queued,
    /// Working.
    Running,
    /// Paused by the user; workers are parked.
    Paused,
    /// Finished successfully.
    Done,
    /// Finished with an error, described by the message.
    Failed(String),
    /// Killed by the user, or its tab was closed.
    Cancelled,
}

impl JobState {
    /// Whether the job has finished (successfully or not).
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed(_) | JobState::Cancelled
        )
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a job did not finish successfully.
#[derive(Debug, Error)]
pub enum JobError {
    /// The job's token was cancelled (killed by the user, superseded by a
    /// newer filter, or its tab was closed). Not shown as a failure.
    #[error("cancelled")]
    Cancelled,
    /// An I/O operation failed; `context` says what was being done
    /// (`"writing run file /var/tmp/tachy-job-x/run-3"`).
    #[error("{context}: {source}")]
    Io {
        /// What the job was doing, for the `✗` line in the jobs drawer.
        context: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Any other failure, already formatted for display.
    #[error("{0}")]
    Other(String),
}

impl JobError {
    /// An [`JobError::Io`] with a context message.
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        JobError::Io {
            context: context.into(),
            source,
        }
    }

    /// Whether this is [`JobError::Cancelled`]. Cancelled jobs are removed
    /// from the jobs list instead of being shown as failed (M5-01).
    pub fn is_cancelled(&self) -> bool {
        matches!(self, JobError::Cancelled)
    }

    /// The [`JobState`] a job that ended with this error moves to.
    pub fn to_state(&self) -> JobState {
        match self {
            JobError::Cancelled => JobState::Cancelled,
            other => JobState::Failed(other.to_string()),
        }
    }
}

impl From<crate::Error> for JobError {
    fn from(e: crate::Error) -> Self {
        match e {
            crate::Error::Cancelled => JobError::Cancelled,
            crate::Error::Io { path, source } => JobError::Io {
                context: path.display().to_string(),
                source,
            },
            other => JobError::Other(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// `done / total` as a fraction in `0.0..=1.0`, or `None` when the total is
/// unknown (0).
fn fraction(done: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (done as f64 / total as f64).min(1.0))
}

/// Byte progress: indexing, stdin spooling, UTF-16 transcoding (§10.1).
#[derive(Debug, Default)]
pub struct BytesProgress {
    /// Bytes processed so far.
    pub done: AtomicU64,
    /// Bytes to process; 0 when unknown (stdin).
    pub total: AtomicU64,
}

impl BytesProgress {
    /// Counters at zero with a known total (0 = unknown).
    pub fn new(total: u64) -> Self {
        BytesProgress {
            done: AtomicU64::new(0),
            total: AtomicU64::new(total),
        }
    }

    /// Adds `n` processed bytes.
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    /// Bytes processed so far.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Total bytes, 0 when unknown.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Completed fraction, `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        fraction(self.done(), self.total())
    }
}

/// Filter progress (M4-04): bytes scanned and matches found.
#[derive(Debug, Default)]
pub struct FilterProgress {
    /// Bytes of the view scanned so far.
    pub bytes_done: AtomicU64,
    /// Bytes to scan; 0 when unknown.
    pub bytes_total: AtomicU64,
    /// Matching rows found so far.
    pub matches: AtomicU64,
}

impl FilterProgress {
    /// Counters at zero with a known byte total.
    pub fn new(bytes_total: u64) -> Self {
        FilterProgress {
            bytes_total: AtomicU64::new(bytes_total),
            ..Self::default()
        }
    }

    /// Adds scanned bytes and matches from one chunk.
    pub fn add(&self, bytes: u64, matches: u64) {
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
        self.matches.fetch_add(matches, Ordering::Relaxed);
    }

    /// Bytes scanned so far.
    pub fn bytes_done(&self) -> u64 {
        self.bytes_done.load(Ordering::Relaxed)
    }

    /// Bytes to scan, 0 when unknown.
    pub fn bytes_total(&self) -> u64 {
        self.bytes_total.load(Ordering::Relaxed)
    }

    /// Matches found so far.
    pub fn matches(&self) -> u64 {
        self.matches.load(Ordering::Relaxed)
    }

    /// Completed fraction, `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        fraction(self.bytes_done(), self.bytes_total())
    }
}

/// The phases of an external merge sort (M5-03), shown as
/// "extract / sort runs / merge pass n/m" (§10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum SortPhase {
    /// Reading the sort keys out of the view.
    #[default]
    Extract = 0,
    /// Sorting in-memory buffers and writing them as runs.
    SortRuns = 1,
    /// k-way merging runs; see [`SortProgress::pass`].
    Merge = 2,
}

impl SortPhase {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => SortPhase::Extract,
            1 => SortPhase::SortRuns,
            _ => SortPhase::Merge,
        }
    }
}

/// Sort progress (M5-03). `done`/`total` count the unit of the current
/// phase (rows), and are reset by [`SortProgress::start_phase`].
#[derive(Debug, Default)]
pub struct SortProgress {
    /// The current [`SortPhase`], as its `u8` value.
    pub phase: AtomicU8,
    /// Units done in the current phase.
    pub done: AtomicU64,
    /// Units in the current phase; 0 when unknown.
    pub total: AtomicU64,
    /// Current merge pass, 1-based (0 outside [`SortPhase::Merge`]).
    pub pass: AtomicU32,
    /// Number of merge passes planned.
    pub passes: AtomicU32,
}

impl SortProgress {
    /// Progress at the start of [`SortPhase::Extract`] with `total` units.
    pub fn new(total: u64) -> Self {
        SortProgress {
            total: AtomicU64::new(total),
            ..Self::default()
        }
    }

    /// Enters `phase`: resets `done` and sets the phase's `total`.
    pub fn start_phase(&self, phase: SortPhase, total: u64) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        self.phase.store(phase as u8, Ordering::Release);
    }

    /// Starts merge pass `pass` of `passes` (1-based) over `total` units.
    pub fn start_merge_pass(&self, pass: u32, passes: u32, total: u64) {
        self.pass.store(pass, Ordering::Relaxed);
        self.passes.store(passes, Ordering::Relaxed);
        self.start_phase(SortPhase::Merge, total);
    }

    /// Adds `n` units done in the current phase.
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    /// The current phase.
    pub fn phase(&self) -> SortPhase {
        SortPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// Units done in the current phase.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Units in the current phase, 0 when unknown.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// `(pass, passes)` of the merge phase.
    pub fn pass(&self) -> (u32, u32) {
        (
            self.pass.load(Ordering::Relaxed),
            self.passes.load(Ordering::Relaxed),
        )
    }

    /// Completed fraction of the current phase.
    pub fn fraction(&self) -> Option<f64> {
        fraction(self.done(), self.total())
    }
}

/// Row progress: profiling (M5-04).
#[derive(Debug, Default)]
pub struct RowsProgress {
    /// Rows processed so far.
    pub done: AtomicU64,
    /// Rows to process; 0 when unknown.
    pub total: AtomicU64,
}

impl RowsProgress {
    /// Counters at zero with a known total (0 = unknown).
    pub fn new(total: u64) -> Self {
        RowsProgress {
            done: AtomicU64::new(0),
            total: AtomicU64::new(total),
        }
    }

    /// Adds `n` processed rows.
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    /// Rows processed so far.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Total rows, 0 when unknown.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Completed fraction, `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        fraction(self.done(), self.total())
    }
}

/// Export progress (M6-01): rows and bytes written.
#[derive(Debug, Default)]
pub struct ExportProgress {
    /// Rows written so far.
    pub rows_written: AtomicU64,
    /// Bytes written so far.
    pub bytes_written: AtomicU64,
    /// Rows to write; 0 when unknown.
    pub total_rows: AtomicU64,
}

impl ExportProgress {
    /// Counters at zero with a known row total (0 = unknown).
    pub fn new(total_rows: u64) -> Self {
        ExportProgress {
            total_rows: AtomicU64::new(total_rows),
            ..Self::default()
        }
    }

    /// Adds rows and bytes written by one batch.
    pub fn add(&self, rows: u64, bytes: u64) {
        self.rows_written.fetch_add(rows, Ordering::Relaxed);
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Rows written so far.
    pub fn rows_written(&self) -> u64 {
        self.rows_written.load(Ordering::Relaxed)
    }

    /// Bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// Rows to write, 0 when unknown.
    pub fn total_rows(&self) -> u64 {
        self.total_rows.load(Ordering::Relaxed)
    }

    /// Completed fraction, `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        fraction(self.rows_written(), self.total_rows())
    }
}

/// Shared progress counters of one job. Workers update them; the UI reads
/// them each frame. No per-update messages (README §A2).
///
/// Cloning shares the counters.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Index, spool, transcode.
    Bytes(Arc<BytesProgress>),
    /// Filter (M4-04).
    Filter(Arc<FilterProgress>),
    /// Sort (M5-03).
    Sort(Arc<SortProgress>),
    /// Profile (M5-04) and duplicates.
    Rows(Arc<RowsProgress>),
    /// Export (M6-01).
    Export(Arc<ExportProgress>),
}

impl Progress {
    /// The progress shape that matches a job kind, with zeroed counters and
    /// `total` as the main unit's total (0 = unknown): bytes for Index and
    /// Filter, rows for Sort, Profile and Export.
    pub fn for_kind(kind: JobKind, total: u64) -> Self {
        match kind {
            JobKind::Index => Progress::Bytes(Arc::new(BytesProgress::new(total))),
            JobKind::Filter => Progress::Filter(Arc::new(FilterProgress::new(total))),
            JobKind::Sort => Progress::Sort(Arc::new(SortProgress::new(total))),
            JobKind::Profile | JobKind::Dupes => Progress::Rows(Arc::new(RowsProgress::new(total))),
            JobKind::Export => Progress::Export(Arc::new(ExportProgress::new(total))),
        }
    }

    /// Completed fraction of the job (of the current phase for a sort), for
    /// gauges. `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        match self {
            Progress::Bytes(p) => p.fraction(),
            Progress::Filter(p) => p.fraction(),
            Progress::Sort(p) => p.fraction(),
            Progress::Rows(p) => p.fraction(),
            Progress::Export(p) => p.fraction(),
        }
    }

    /// The main "done" counter (bytes, rows), for tests and rate estimates.
    pub fn done(&self) -> u64 {
        match self {
            Progress::Bytes(p) => p.done(),
            Progress::Filter(p) => p.bytes_done(),
            Progress::Sort(p) => p.done(),
            Progress::Rows(p) => p.done(),
            Progress::Export(p) => p.rows_written(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pause, cancellation, checkpoints
// ---------------------------------------------------------------------------

/// How often blocking workers must call [`Checkpoint::check`]: at least
/// every 64 KiB of input (§4.3).
pub const CHECK_INTERVAL: usize = 64 * 1024;

/// Longest a paused worker sleeps before re-checking cancellation, so a
/// missed wake-up can never leave it stuck.
pub const PAUSE_POLL: Duration = Duration::from_millis(100);

/// Pause flag of one job (§10.3 `p`). Blocking workers park on a `Condvar`
/// while it is set; they run on tokio's blocking pool, so parking is fine.
///
/// Cloning shares the flag.
#[derive(Debug, Clone, Default)]
pub struct PauseToken(Arc<(Mutex<bool>, Condvar)>);

impl PauseToken {
    /// A token that is not paused.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks workers to park at their next checkpoint.
    pub fn pause(&self) {
        *self.lock() = true;
    }

    /// Clears the flag and wakes every parked worker. Also called after
    /// cancelling a job, so paused workers see the cancellation at once.
    pub fn resume(&self) {
        *self.lock() = false;
        self.0.1.notify_all();
    }

    /// Whether the job is paused.
    pub fn is_paused(&self) -> bool {
        *self.lock()
    }

    /// Blocks while paused. Returns on resume, or when `cancel` is cancelled:
    /// the wait wakes every [`PAUSE_POLL`] (100 ms) to re-check it, so
    /// cancelling a paused job without a `resume` still ends it.
    ///
    /// Call from blocking code only (never from an async task).
    pub fn wait_while_paused(&self, cancel: &CancellationToken) {
        let (_, cvar) = &*self.0;
        let mut paused = self.lock();
        while *paused && !cancel.is_cancelled() {
            paused = cvar
                .wait_timeout(paused, PAUSE_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// What every blocking worker calls at least every [`CHECK_INTERVAL`] bytes
/// of input (§4.3).
#[derive(Debug, Clone, Copy)]
pub struct Checkpoint<'a> {
    cancel: &'a CancellationToken,
    pause: &'a PauseToken,
}

impl<'a> Checkpoint<'a> {
    /// A checkpoint over a job's tokens.
    pub fn new(cancel: &'a CancellationToken, pause: &'a PauseToken) -> Self {
        Checkpoint { cancel, pause }
    }

    /// Waits while the job is paused, then returns `Err(Cancelled)` if it was
    /// cancelled (before or during the pause), `Ok(())` otherwise.
    pub fn check(&self) -> Result<(), JobError> {
        if self.cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        self.pause.wait_while_paused(self.cancel);
        if self.cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        Ok(())
    }

    /// Whether the job was cancelled, without waiting on a pause.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// The owned pair of tokens of one job, cheap to clone into
/// `spawn_blocking` closures. [`JobControl::check`] is
/// [`Checkpoint::check`].
#[derive(Debug, Clone, Default)]
pub struct JobControl {
    /// Cancelled by `K`, a superseding filter, or closing the tab.
    pub cancel: CancellationToken,
    /// Set by `p`.
    pub pause: PauseToken,
}

impl JobControl {
    /// Tokens for a job, given its (child) cancellation token.
    pub fn new(cancel: CancellationToken) -> Self {
        JobControl {
            cancel,
            pause: PauseToken::new(),
        }
    }

    /// A borrowed [`Checkpoint`] over these tokens.
    pub fn checkpoint(&self) -> Checkpoint<'_> {
        Checkpoint::new(&self.cancel, &self.pause)
    }

    /// See [`Checkpoint::check`].
    pub fn check(&self) -> Result<(), JobError> {
        self.checkpoint().check()
    }

    /// Cancels the job and wakes its paused workers (cancel, then resume, as
    /// `JobManager::cancel` must do).
    pub fn cancel(&self) {
        self.cancel.cancel();
        self.pause.resume();
    }
}

// ---------------------------------------------------------------------------
// Memory budget
// ---------------------------------------------------------------------------

/// One mebibyte.
pub const MIB: u64 = 1 << 20;
/// Default `--mem` (§3): 2 GiB.
pub const DEFAULT_MEMORY: u64 = 2048 * MIB;
/// Fixed share of the row cache and the UI (§10.4).
pub const RESERVED_UI: u64 = 128 * MIB;
/// Smallest buffer a sort starts with (unless it needs less).
pub const SORT_MIN: u64 = 64 * MIB;
/// Budget of a filter: local bitmaps and the reorder buffer.
pub const FILTER_BUDGET: u64 = 64 * MIB;
/// Budget of a profile: top-k sketches (M5-04).
pub const PROFILE_BUDGET: u64 = 256 * MIB;
/// Budget of an export: output buffers.
pub const EXPORT_BUDGET: u64 = 32 * MIB;

/// What a job asks the [`MemoryBudget`] for when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetRequest {
    /// 25 % of what is free; never queues (indexing is not scheduled).
    Index,
    /// [`FILTER_BUDGET`].
    Filter,
    /// `min(0.75 × free, needed)`, at least `min(SORT_MIN, needed)`.
    Sort {
        /// Bytes the sort would use to run fully in memory.
        needed: u64,
    },
    /// [`PROFILE_BUDGET`].
    Profile,
    /// [`EXPORT_BUDGET`].
    Export,
    /// Like `Sort`: `needed` is the in-memory size of the key records.
    Dupes {
        /// Bytes the job would use to group fully in memory.
        needed: u64,
    },
}

impl BudgetRequest {
    /// The request for a job kind; `needed` is only used by Sort and Dupes.
    pub fn for_kind(kind: JobKind, needed: u64) -> Self {
        match kind {
            JobKind::Index => BudgetRequest::Index,
            JobKind::Filter => BudgetRequest::Filter,
            JobKind::Sort => BudgetRequest::Sort { needed },
            JobKind::Profile => BudgetRequest::Profile,
            JobKind::Export => BudgetRequest::Export,
            JobKind::Dupes => BudgetRequest::Dupes { needed },
        }
    }

    /// The job kind making the request.
    pub fn kind(&self) -> JobKind {
        match self {
            BudgetRequest::Index => JobKind::Index,
            BudgetRequest::Filter => JobKind::Filter,
            BudgetRequest::Sort { .. } => JobKind::Sort,
            BudgetRequest::Profile => JobKind::Profile,
            BudgetRequest::Export => JobKind::Export,
            BudgetRequest::Dupes { .. } => JobKind::Dupes,
        }
    }

    /// The least the job can start with; below it, the job queues.
    pub fn minimum(&self) -> u64 {
        match *self {
            BudgetRequest::Index => 0,
            BudgetRequest::Filter => FILTER_BUDGET,
            BudgetRequest::Sort { needed } | BudgetRequest::Dupes { needed } => {
                SORT_MIN.min(needed)
            }
            BudgetRequest::Profile => PROFILE_BUDGET,
            BudgetRequest::Export => EXPORT_BUDGET,
        }
    }
}

/// The global `--mem` budget (§10.4), owned by the `JobManager` and divided
/// at job start: `free = total − reserved_ui − Σ(running jobs' budgets)`.
///
/// Advisory only: jobs size their buffers from their grant; nothing is
/// enforced at the allocator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBudget {
    total: u64,
    reserved_ui: u64,
    in_use: u64,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MEMORY)
    }
}

impl MemoryBudget {
    /// A budget of `total` bytes (`--mem`), with [`RESERVED_UI`] set aside.
    pub fn new(total: u64) -> Self {
        MemoryBudget {
            total,
            reserved_ui: RESERVED_UI,
            in_use: 0,
        }
    }

    /// `--mem`.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The fixed share of the row cache and UI.
    pub fn reserved_ui(&self) -> u64 {
        self.reserved_ui
    }

    /// Sum of the grants of running jobs.
    pub fn in_use(&self) -> u64 {
        self.in_use
    }

    /// What is left for new jobs (never negative).
    pub fn free(&self) -> u64 {
        self.total
            .saturating_sub(self.reserved_ui)
            .saturating_sub(self.in_use)
    }

    /// What `req` would be granted now, without reserving it. `None` when
    /// less than the request's minimum is free: the job stays `Queued`.
    pub fn grant_for(&self, req: BudgetRequest) -> Option<u64> {
        let free = self.free();
        let grant = match req {
            BudgetRequest::Index => free / 4,
            BudgetRequest::Sort { needed } | BudgetRequest::Dupes { needed } => {
                // 75 % of free, without overflow.
                let three_quarters = (u128::from(free) * 3 / 4) as u64;
                three_quarters.min(needed).max(req.minimum())
            }
            BudgetRequest::Filter | BudgetRequest::Profile | BudgetRequest::Export => req.minimum(),
        };
        (grant <= free).then_some(grant)
    }

    /// Reserves and returns the grant for `req` (see
    /// [`MemoryBudget::grant_for`]), or `None` if the job must queue.
    /// Give the grant back with [`MemoryBudget::release`] when the job ends.
    pub fn try_allocate(&mut self, req: BudgetRequest) -> Option<u64> {
        let grant = self.grant_for(req)?;
        self.in_use += grant;
        Some(grant)
    }

    /// Returns a grant when its job finishes, fails or is cancelled.
    pub fn release(&mut self, bytes: u64) {
        debug_assert!(bytes <= self.in_use, "releasing more than was granted");
        self.in_use = self.in_use.saturating_sub(bytes);
    }

    /// Changes `--mem` (e.g. after a config reload). Running grants are kept;
    /// `free` may drop to 0 until they are released.
    pub fn set_total(&mut self, total: u64) {
        self.total = total;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::AtomicBool,
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    /// A fake blocking worker: loops on `check()`, counting iterations.
    fn spawn_worker(
        ctl: JobControl,
        progress: Arc<RowsProgress>,
    ) -> thread::JoinHandle<Result<(), JobError>> {
        thread::spawn(move || {
            loop {
                ctl.check()?;
                progress.add(1);
                thread::sleep(Duration::from_micros(200));
            }
        })
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        cond()
    }

    #[test]
    fn job_error_states_and_display() {
        assert!(JobError::Cancelled.is_cancelled());
        assert_eq!(JobError::Cancelled.to_state(), JobState::Cancelled);
        let e = JobError::io("writing run file", io::Error::other("disk full"));
        assert_eq!(e.to_string(), "writing run file: disk full");
        assert_eq!(
            e.to_state(),
            JobState::Failed("writing run file: disk full".into())
        );
        assert!(!JobError::Other("x".into()).is_cancelled());
        assert!(JobError::from(crate::Error::Cancelled).is_cancelled());
        let io = JobError::from(crate::Error::Io {
            path: "/tmp/a".into(),
            source: io::Error::other("nope"),
        });
        assert_eq!(io.to_string(), "/tmp/a: nope");
    }

    #[test]
    fn progress_counters() {
        let p = Progress::for_kind(JobKind::Index, 200);
        let Progress::Bytes(b) = &p else {
            panic!("index uses bytes")
        };
        b.add(50);
        assert_eq!(p.done(), 50);
        assert_eq!(p.fraction(), Some(0.25));
        // Shared through clones.
        let q = p.clone();
        b.add(150);
        assert_eq!(q.fraction(), Some(1.0));
        b.add(100);
        assert_eq!(q.fraction(), Some(1.0), "clamped");

        let unknown = BytesProgress::new(0);
        unknown.add(10);
        assert_eq!(unknown.fraction(), None);

        let f = FilterProgress::new(100);
        f.add(40, 3);
        f.add(10, 2);
        assert_eq!((f.bytes_done(), f.matches()), (50, 5));
        assert_eq!(f.fraction(), Some(0.5));

        let e = ExportProgress::new(10);
        e.add(5, 500);
        assert_eq!((e.rows_written(), e.bytes_written()), (5, 500));
        assert_eq!(e.fraction(), Some(0.5));

        let r = RowsProgress::new(4);
        r.add(1);
        assert_eq!(r.fraction(), Some(0.25));

        assert!(matches!(
            Progress::for_kind(JobKind::Filter, 0),
            Progress::Filter(_)
        ));
        assert!(matches!(
            Progress::for_kind(JobKind::Profile, 0),
            Progress::Rows(_)
        ));
        assert!(matches!(
            Progress::for_kind(JobKind::Export, 0),
            Progress::Export(_)
        ));
        assert!(matches!(
            Progress::for_kind(JobKind::Dupes, 0),
            Progress::Rows(_)
        ));
    }

    #[test]
    fn sort_progress_phases() {
        let s = SortProgress::new(1000);
        assert_eq!(s.phase(), SortPhase::Extract);
        s.add(1000);
        assert_eq!(s.fraction(), Some(1.0));
        s.start_phase(SortPhase::SortRuns, 10);
        assert_eq!(
            (s.phase(), s.done(), s.total()),
            (SortPhase::SortRuns, 0, 10)
        );
        s.start_merge_pass(2, 3, 1000);
        assert_eq!(s.phase(), SortPhase::Merge);
        assert_eq!(s.pass(), (2, 3));
        assert_eq!(s.done(), 0);
        assert!(matches!(
            Progress::for_kind(JobKind::Sort, 5),
            Progress::Sort(_)
        ));
    }

    #[test]
    fn checkpoint_ok_and_cancelled() {
        let cancel = CancellationToken::new();
        let pause = PauseToken::new();
        let cp = Checkpoint::new(&cancel, &pause);
        assert!(cp.check().is_ok());
        assert!(!cp.is_cancelled());
        cancel.cancel();
        assert!(cp.check().unwrap_err().is_cancelled());
        assert!(cp.is_cancelled());
    }

    #[test]
    fn child_token_cancelled_by_tab() {
        let tab = CancellationToken::new();
        let ctl = JobControl::new(tab.child_token());
        assert!(ctl.check().is_ok());
        tab.cancel();
        assert!(ctl.check().unwrap_err().is_cancelled());
    }

    #[test]
    fn pause_flag() {
        let p = PauseToken::new();
        assert!(!p.is_paused());
        p.pause();
        assert!(p.is_paused());
        assert!(p.clone().is_paused(), "clones share the flag");
        p.resume();
        assert!(!p.is_paused());
        // Not paused: returns at once.
        p.wait_while_paused(&CancellationToken::new());
    }

    #[test]
    fn pause_stops_progress_and_resume_continues() {
        let ctl = JobControl::default();
        let progress = Arc::new(RowsProgress::new(0));
        let worker = spawn_worker(ctl.clone(), Arc::clone(&progress));
        assert!(wait_until(|| progress.done() > 10, Duration::from_secs(5)));

        ctl.pause.pause();
        thread::sleep(Duration::from_millis(50)); // let it reach a checkpoint
        let parked_at = progress.done();
        thread::sleep(Duration::from_millis(250)); // > 2 pause polls
        assert_eq!(
            progress.done(),
            parked_at,
            "a paused worker makes no progress"
        );

        ctl.pause.resume();
        assert!(wait_until(
            || progress.done() > parked_at + 10,
            Duration::from_secs(5)
        ));
        ctl.cancel();
        assert!(worker.join().unwrap().unwrap_err().is_cancelled());
    }

    #[test]
    fn cancel_wakes_a_paused_worker() {
        let ctl = JobControl::default();
        let progress = Arc::new(RowsProgress::new(0));
        let worker = spawn_worker(ctl.clone(), Arc::clone(&progress));
        assert!(wait_until(|| progress.done() > 0, Duration::from_secs(5)));
        ctl.pause.pause();
        thread::sleep(Duration::from_millis(50));

        // Cancel + resume, as the JobManager does: ends well within 100 ms.
        let start = Instant::now();
        ctl.cancel();
        let r = worker.join().unwrap();
        assert!(r.unwrap_err().is_cancelled());
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn cancel_without_resume_still_wakes_within_the_poll() {
        let cancel = CancellationToken::new();
        let pause = PauseToken::new();
        pause.pause();
        let done = Arc::new(AtomicBool::new(false));
        let worker = {
            let (cancel, pause, done) = (cancel.clone(), pause.clone(), Arc::clone(&done));
            thread::spawn(move || {
                let r = Checkpoint::new(&cancel, &pause).check();
                done.store(true, Ordering::SeqCst);
                r
            })
        };
        thread::sleep(Duration::from_millis(30));
        assert!(!done.load(Ordering::SeqCst), "parked while paused");
        let start = Instant::now();
        cancel.cancel(); // no resume: a "missed wake-up"
        assert!(worker.join().unwrap().unwrap_err().is_cancelled());
        let waited = start.elapsed();
        assert!(
            waited <= PAUSE_POLL + Duration::from_millis(100),
            "{waited:?}"
        );
        assert!(pause.is_paused(), "the flag itself is untouched");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_wakes_a_paused_blocking_task() {
        let ctl = JobControl::new(CancellationToken::new());
        ctl.pause.pause();
        let task = tokio::task::spawn_blocking({
            let ctl = ctl.clone();
            move || ctl.check()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        ctl.cancel();
        let r = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("woken within 100 ms")
            .unwrap();
        assert!(r.unwrap_err().is_cancelled());
    }

    // ---- memory budget -----------------------------------------------------

    #[test]
    fn budget_defaults() {
        let b = MemoryBudget::default();
        assert_eq!(b.total(), 2048 * MIB);
        assert_eq!(b.reserved_ui(), 128 * MIB);
        assert_eq!(b.in_use(), 0);
        assert_eq!(b.free(), 1920 * MIB);
    }

    #[test]
    fn budget_fixed_kinds() {
        let mut b = MemoryBudget::new(2048 * MIB);
        assert_eq!(b.try_allocate(BudgetRequest::Filter), Some(64 * MIB));
        assert_eq!(b.try_allocate(BudgetRequest::Profile), Some(256 * MIB));
        assert_eq!(b.try_allocate(BudgetRequest::Export), Some(32 * MIB));
        assert_eq!(b.in_use(), 352 * MIB);
        assert_eq!(b.free(), (1920 - 352) * MIB);
        b.release(256 * MIB);
        assert_eq!(b.in_use(), 96 * MIB);
    }

    #[test]
    fn budget_sort_takes_three_quarters_of_free() {
        let mut b = MemoryBudget::new(2048 * MIB);
        // Needs more than is free: 75 % of 1920 MiB.
        assert_eq!(
            b.try_allocate(BudgetRequest::Sort { needed: 10 << 30 }),
            Some(1440 * MIB)
        );
        assert_eq!(b.free(), 480 * MIB);
        // A second sort (other file) gets 75 % of what is left.
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: u64::MAX }),
            Some(360 * MIB)
        );
    }

    #[test]
    fn budget_dupes_is_sized_like_a_sort() {
        let b = MemoryBudget::new(2048 * MIB);
        assert_eq!(
            b.grant_for(BudgetRequest::Dupes { needed: 10 << 30 }),
            Some(1440 * MIB)
        );
        assert_eq!(
            b.grant_for(BudgetRequest::Dupes { needed: 5 * MIB }),
            Some(5 * MIB)
        );
        assert_eq!(BudgetRequest::Dupes { needed: 1 << 30 }.minimum(), SORT_MIN);
        assert_eq!(
            BudgetRequest::for_kind(JobKind::Dupes, 7),
            BudgetRequest::Dupes { needed: 7 }
        );
    }

    #[test]
    fn budget_sort_takes_only_what_it_needs() {
        let b = MemoryBudget::new(2048 * MIB);
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: 100 * MIB }),
            Some(100 * MIB)
        );
        // Small sorts need less than the 64 MiB minimum.
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: 5 * MIB }),
            Some(5 * MIB)
        );
        assert_eq!(BudgetRequest::Sort { needed: 5 * MIB }.minimum(), 5 * MIB);
    }

    #[test]
    fn budget_sort_minimum_and_queueing() {
        // free = 80 MiB: 75 % = 60 MiB < 64 MiB minimum → raised to 64 MiB.
        let b = MemoryBudget::new(208 * MIB);
        assert_eq!(b.free(), 80 * MIB);
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: 1 << 30 }),
            Some(64 * MIB)
        );
        // free = 63 MiB: below the minimum → queue.
        let b = MemoryBudget::new(191 * MIB);
        assert_eq!(b.grant_for(BudgetRequest::Sort { needed: 1 << 30 }), None);
    }

    #[test]
    fn budget_queues_below_minimum_until_release() {
        let mut b = MemoryBudget::new(512 * MIB); // free = 384 MiB
        let profile = b.try_allocate(BudgetRequest::Profile).unwrap(); // 256
        let filter = b.try_allocate(BudgetRequest::Filter).unwrap(); // 64
        assert_eq!(b.free(), 64 * MIB);
        // Another profile does not fit; the job stays queued.
        assert_eq!(b.try_allocate(BudgetRequest::Profile), None);
        assert_eq!(b.in_use(), 320 * MIB, "a refused request reserves nothing");
        // Exactly the minimum still fits.
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: 1 << 30 }),
            Some(64 * MIB)
        );
        b.release(filter);
        b.release(profile);
        assert_eq!(b.try_allocate(BudgetRequest::Profile), Some(256 * MIB));
    }

    #[test]
    fn budget_index_gets_a_quarter_and_never_queues() {
        let mut b = MemoryBudget::new(2048 * MIB);
        assert_eq!(b.try_allocate(BudgetRequest::Index), Some(480 * MIB));
        assert_eq!(b.free(), 1440 * MIB);
        // Nothing free: still starts (with 0, the indexer uses its floor).
        let mut tiny = MemoryBudget::new(64 * MIB);
        assert_eq!(tiny.free(), 0);
        assert_eq!(tiny.try_allocate(BudgetRequest::Index), Some(0));
        assert_eq!(tiny.try_allocate(BudgetRequest::Export), None);
    }

    #[test]
    fn budget_various_mem_values() {
        for (mem, filter, profile, export, sort) in [
            // --mem below the UI reserve: nothing fits.
            (100 * MIB, None, None, None, None),
            (
                256 * MIB,
                Some(64 * MIB),
                None,
                Some(32 * MIB),
                Some(96 * MIB),
            ),
            (
                1024 * MIB,
                Some(64 * MIB),
                Some(256 * MIB),
                Some(32 * MIB),
                Some(672 * MIB),
            ),
            (
                8192 * MIB,
                Some(64 * MIB),
                Some(256 * MIB),
                Some(32 * MIB),
                Some(6048 * MIB),
            ),
        ] {
            let b = MemoryBudget::new(mem);
            assert_eq!(b.grant_for(BudgetRequest::Filter), filter, "{mem}");
            assert_eq!(b.grant_for(BudgetRequest::Profile), profile, "{mem}");
            assert_eq!(b.grant_for(BudgetRequest::Export), export, "{mem}");
            assert_eq!(
                b.grant_for(BudgetRequest::Sort { needed: u64::MAX }),
                sort,
                "{mem}"
            );
        }
    }

    #[test]
    fn budget_mix_of_running_jobs() {
        let mut b = MemoryBudget::new(1024 * MIB); // free = 896 MiB
        let grants: Vec<u64> = [
            BudgetRequest::Index,                     // 224 → 672 left
            BudgetRequest::Filter,                    // 64 → 608
            BudgetRequest::Export,                    // 32 → 576
            BudgetRequest::Sort { needed: u64::MAX }, // 432 → 144
            BudgetRequest::Filter,                    // 64 → 80
        ]
        .into_iter()
        .map(|r| b.try_allocate(r).unwrap())
        .collect();
        assert_eq!(grants, [224 * MIB, 64 * MIB, 32 * MIB, 432 * MIB, 64 * MIB]);
        assert_eq!(b.free(), 80 * MIB);
        assert_eq!(b.try_allocate(BudgetRequest::Profile), None);
        // 80 MiB free: 75 % = 60 → minimum 64.
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: u64::MAX }),
            Some(64 * MIB)
        );
        for g in grants {
            b.release(g);
        }
        assert_eq!(b.free(), 896 * MIB);
    }

    #[test]
    fn budget_set_total_and_request_helpers() {
        let mut b = MemoryBudget::new(2048 * MIB);
        let g = b.try_allocate(BudgetRequest::Profile).unwrap();
        b.set_total(256 * MIB);
        assert_eq!(b.free(), 0);
        b.release(g);
        assert_eq!(b.free(), 128 * MIB);
        for kind in [
            JobKind::Index,
            JobKind::Filter,
            JobKind::Sort,
            JobKind::Profile,
            JobKind::Export,
            JobKind::Dupes,
        ] {
            assert_eq!(BudgetRequest::for_kind(kind, 1).kind(), kind);
        }
        assert_eq!(BudgetRequest::Index.minimum(), 0);
    }
}
