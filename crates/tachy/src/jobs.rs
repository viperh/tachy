//! UI side of background jobs (spec §10, M5-01): the [`JobManager`] that
//! runs, queues, pauses, cancels and retires jobs, under the concurrency
//! rules of §10.3 and the memory budget of §10.4.
//!
//! The core side (tokens, progress counters, the budget arithmetic) is in
//! `tachy_core::jobs`. A job is an `async` future built by its
//! [`JobSpec::start`] closure from a [`JobContext`]; the manager
//! `tokio::spawn`s it and the task sends `Msg::Job(JobMsg::Finished)` when it
//! ends, which the app hands back to [`JobManager::finished`].
//!
//! # Rules (§10.3)
//!
//! - At most **one Sort**, **one Export**, **one Profile** and **one Dupes**
//!   running (or paused) per tab. Extra ones stay `Queued` and start in FIFO
//!   order when the slot frees. Profile and Dupes are not in the spec: one
//!   at a time protects the budget.
//! - A new Filter cancels the running (or queued) filters that read the
//!   **same parent view** of the same tab, and no others.
//! - A job whose minimum budget isn't free stays `Queued` until a running job
//!   finishes ([`MemoryBudget::try_allocate`]).
//! - Index jobs are not scheduled: the indexer registers itself for display
//!   ([`JobManager::register_index`]) and always gets its grant (25 % of
//!   what's free, possibly 0).
//!
//! # Lifecycle and retention (§10.2)
//!
//! `Queued → Running ⇄ Paused → Done | Failed | Cancelled`. Done and Failed
//! jobs stay listed until dismissed or until more than [`KEEP_FINISHED`]
//! finished jobs exist (the oldest goes). A cancelled job disappears from the
//! list at once: it is marked `Cancelled` (hidden) until its task ends, then
//! removed, and its result, if it raced the cancel, is dropped. Index jobs
//! are removed when the index finishes.
//!
//! # Cleanup
//!
//! Temp files are owned by RAII values inside the job future
//! (each core job makes its own `tachy-job-*` directory in `--tmp`), so
//! dropping the future after a cancel or a failure deletes them, panics
//! included. Nothing is tracked by path.

use std::{
    fmt,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use tachy_core::{
    dupes::{DupeResult, DupeSpec},
    exec::Executor,
    jobs::{
        BudgetRequest, JobControl, JobError, JobKind, JobState, MemoryBudget, PauseToken, Progress,
        SortPhase,
    },
    sort::SortKey,
    stats::profile::ProfileResult,
    view::RowIdList,
};
use tokio::{
    sync::mpsc::UnboundedSender,
    task::{AbortHandle, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use crate::{msg::Msg, tab::TabId};

/// Finished (Done / Failed) jobs kept in the list (§10.2: "until 10 newer
/// jobs exist").
pub const KEEP_FINISHED: usize = 10;
/// How long quitting waits for cancelled jobs to drop their temp files.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Identifies a job for the lifetime of the app. Never reused.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// An index into a tab's `ViewStack` (0 is the `All` view).
pub type ViewKey = usize;

/// What a successful job delivers. The app applies it to its tab or view
/// when `JobMsg::Finished` arrives; if the tab is gone it is dropped.
#[allow(clippy::enum_variant_names)] // `…Done` reads well at the match sites.
pub enum JobOutput {
    /// The filter's rows are already live in its view (M4-04).
    FilterDone,
    /// The permutation file of a sort, and its keys (M5-03): pushed as an
    /// `Ordered { Sorted }` view.
    SortDone {
        list: Arc<RowIdList>,
        keys: Vec<SortKey>,
    },
    /// Full-column statistics (M5-04).
    ProfileDone(ProfileResult),
    /// The export's final path and data rows (M6-01).
    ExportDone { path: PathBuf, rows: u64 },
    /// The rows selected by `dupes` / `dedupe`, and the parent's sort keys
    /// (an `Ordered` parent keeps its order): pushed as a duplicates view.
    DupesDone {
        result: DupeResult,
        spec: DupeSpec,
        keys: Vec<SortKey>,
    },
}

impl fmt::Debug for JobOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobOutput::FilterDone => f.write_str("FilterDone"),
            JobOutput::SortDone { list, keys } => f
                .debug_struct("SortDone")
                .field("rows", &list.len())
                .field("keys", keys)
                .finish(),
            JobOutput::ProfileDone(r) => f
                .debug_struct("ProfileDone")
                .field("columns", &r.columns.len())
                .finish(),
            JobOutput::ExportDone { path, rows } => f
                .debug_struct("ExportDone")
                .field("path", path)
                .field("rows", rows)
                .finish(),
            JobOutput::DupesDone { result, spec, .. } => f
                .debug_struct("DupesDone")
                .field("rows", &result.rows.len())
                .field("groups", &result.groups)
                .field("mode", &spec.mode)
                .finish(),
        }
    }
}

/// Messages from job tasks to the UI task.
#[derive(Debug)]
pub enum JobMsg {
    /// The job's future ended (or panicked: `JobError::Other`).
    Finished {
        id: JobId,
        result: Result<JobOutput, JobError>,
    },
}

/// What a job's future gets when it starts.
#[derive(Clone)]
pub struct JobContext {
    pub exec: Executor,
    /// A child of the tab's token.
    pub cancel: CancellationToken,
    pub pause: PauseToken,
    /// The same counters the drawer reads.
    pub progress: Progress,
    /// The memory grant, in bytes (advisory, §10.4).
    pub budget: u64,
    /// `--tmp`.
    pub tmp_dir: PathBuf,
}

impl JobContext {
    /// The owned token pair for `spawn_blocking` closures.
    pub fn control(&self) -> JobControl {
        JobControl {
            cancel: self.cancel.clone(),
            pause: self.pause.clone(),
        }
    }

    /// This job's temp directory (`tachy-job-*` in `--tmp`). Keep it inside
    /// the future: dropping it deletes everything in it. The core jobs make
    /// their own; tests use this one.
    #[cfg(test)]
    pub fn temp_dir(&self) -> std::io::Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix("tachy-job-")
            .tempdir_in(&self.tmp_dir)
    }
}

/// The future of a job.
pub type JobFuture = Pin<Box<dyn Future<Output = Result<JobOutput, JobError>> + Send>>;
/// Builds the future when the job starts (immediately, or when it leaves the
/// queue).
pub type StartFn = Box<dyn FnOnce(JobContext) -> JobFuture + Send>;

/// A job to submit.
pub struct JobSpec {
    pub kind: JobKind,
    pub tab: TabId,
    /// The view this job produces or feeds (a filter's own view), if any:
    /// killing a filter pops it.
    pub view: Option<ViewKey>,
    /// The view the job reads; filters on the same parent replace each
    /// other.
    pub parent: Option<ViewKey>,
    /// `filter country == "DE"`, `sort price:desc`, `export orders.csv`.
    pub title: String,
    pub progress: Progress,
    /// Bytes a sort would use to run fully in memory (other kinds ignore it).
    pub needed: u64,
    /// A child of the tab's token.
    pub cancel: CancellationToken,
    pub start: StartFn,
}

/// One job in the list.
pub struct JobHandle {
    pub id: JobId,
    pub kind: JobKind,
    pub tab: TabId,
    pub view: Option<ViewKey>,
    pub parent: Option<ViewKey>,
    pub title: String,
    pub state: JobState,
    pub progress: Progress,
    pub cancel: CancellationToken,
    pub pause: PauseToken,
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
    /// The memory grant while running (0 otherwise).
    pub mem_budget: u64,
    needed: u64,
    task: Option<JoinHandle<()>>,
    abort: Option<AbortHandle>,
    start: Option<StartFn>,
}

impl fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobHandle")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("tab", &self.tab)
            .field("title", &self.title)
            .field("state", &self.state)
            .field("mem_budget", &self.mem_budget)
            .finish_non_exhaustive()
    }
}

impl JobHandle {
    /// Running or paused: holds a slot and a grant.
    pub fn is_active(&self) -> bool {
        matches!(self.state, JobState::Running | JobState::Paused)
    }

    /// Running, paused or queued: work the user would lose by quitting.
    pub fn is_pending(&self) -> bool {
        matches!(
            self.state,
            JobState::Running | JobState::Paused | JobState::Queued
        )
    }

    /// Shown in the drawer: everything except cancelled jobs whose task
    /// hasn't ended yet.
    pub fn is_listed(&self) -> bool {
        self.state != JobState::Cancelled
    }

    /// Time since start (or the total run time once finished).
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        let start = self.started?;
        Some(
            self.finished
                .unwrap_or(now)
                .saturating_duration_since(start),
        )
    }

    /// Overall progress in `0..=1`, for a sort across its phases: extract
    /// is the first third, sorting runs the second, merging the last (split
    /// by pass). `None` when the total is unknown.
    pub fn overall_fraction(&self) -> Option<f64> {
        match &self.progress {
            Progress::Sort(p) => {
                let f = p.fraction()?;
                Some(match p.phase() {
                    SortPhase::Extract => f / 3.0,
                    SortPhase::SortRuns => (1.0 + f) / 3.0,
                    SortPhase::Merge => {
                        let (pass, passes) = p.pass();
                        let passes = f64::from(passes.max(1));
                        let done = f64::from(pass.saturating_sub(1)) + f;
                        (2.0 + (done / passes).min(1.0)) / 3.0
                    }
                })
            }
            other => other.fraction(),
        }
    }
}

/// What [`JobManager::finished`] hands back to the app.
#[allow(dead_code)] // `id`, `kind`, `view`: for the result handlers of M4-04 / M5-03.
#[derive(Debug)]
pub struct Finished {
    pub id: JobId,
    pub kind: JobKind,
    pub tab: TabId,
    pub view: Option<ViewKey>,
    pub title: String,
    /// `None` when the job had been cancelled (the result is dropped).
    pub result: Option<Result<JobOutput, JobError>>,
}

/// Every job of the app, from all tabs (README §A3: owned by `AppState`).
pub struct JobManager {
    jobs: Vec<JobHandle>,
    next_id: u64,
    budget: MemoryBudget,
    tmp_dir: PathBuf,
    exec: Option<Executor>,
    msg_tx: Option<UnboundedSender<Msg>>,
}

impl fmt::Debug for JobManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobManager")
            .field("jobs", &self.jobs)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl JobManager {
    /// A manager with a `--mem` budget of `total` bytes, putting job temp
    /// directories in `tmp_dir`.
    pub fn new(total: u64, tmp_dir: PathBuf) -> Self {
        JobManager {
            jobs: Vec::new(),
            next_id: 1,
            budget: MemoryBudget::new(total),
            tmp_dir,
            exec: None,
            msg_tx: None,
        }
    }

    /// Where finished jobs report (`App` sets it once).
    pub fn set_sender(&mut self, tx: UnboundedSender<Msg>) {
        self.msg_tx = Some(tx);
    }

    pub fn budget(&self) -> &MemoryBudget {
        &self.budget
    }

    fn new_id(&mut self) -> JobId {
        let id = JobId(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn get(&self, id: JobId) -> Option<&JobHandle> {
        self.jobs.iter().find(|j| j.id == id)
    }

    fn position(&self, id: JobId) -> Option<usize> {
        self.jobs.iter().position(|j| j.id == id)
    }

    /// Every job, in submission order (cancelled ones included).
    pub fn iter(&self) -> impl Iterator<Item = &JobHandle> {
        self.jobs.iter()
    }

    /// Submits a job (§10.3): applies the rules, then starts or queues it.
    /// Must run inside the runtime. `exec` is kept for queued jobs.
    pub fn submit(&mut self, spec: JobSpec, exec: Executor) -> JobId {
        self.exec = Some(exec);
        if spec.kind == JobKind::Filter {
            let same_view: Vec<JobId> = self
                .jobs
                .iter()
                .filter(|j| {
                    j.kind == JobKind::Filter
                        && j.tab == spec.tab
                        && j.parent == spec.parent
                        && j.is_pending()
                })
                .map(|j| j.id)
                .collect();
            for id in same_view {
                self.cancel(id);
            }
        }
        let id = self.new_id();
        self.jobs.push(JobHandle {
            id,
            kind: spec.kind,
            tab: spec.tab,
            view: spec.view,
            parent: spec.parent,
            title: spec.title,
            state: JobState::Queued,
            progress: spec.progress,
            pause: PauseToken::new(),
            cancel: spec.cancel,
            started: None,
            finished: None,
            mem_budget: 0,
            needed: spec.needed,
            task: None,
            abort: None,
            start: Some(spec.start),
        });
        self.schedule();
        id
    }

    /// Registers a tab's indexer for display (§10.1). It is never queued;
    /// its grant (25 % of what's free) counts while it runs. End it with
    /// [`JobManager::end_index`].
    pub fn register_index(
        &mut self,
        tab: TabId,
        title: String,
        progress: Progress,
        cancel: CancellationToken,
    ) -> JobId {
        let id = self.new_id();
        let grant = self.budget.try_allocate(BudgetRequest::Index).unwrap_or(0);
        self.jobs.push(JobHandle {
            id,
            kind: JobKind::Index,
            tab,
            view: None,
            parent: None,
            title,
            state: JobState::Running,
            progress,
            pause: PauseToken::new(),
            cancel,
            started: Some(crate::app::now()),
            finished: None,
            mem_budget: grant,
            needed: 0,
            task: None,
            abort: None,
            start: None,
        });
        id
    }

    /// Removes the index job of `tab` (index ready, failed or restarted) and
    /// releases its grant. Queued jobs may start.
    pub fn end_index(&mut self, tab: TabId) {
        let mut released = false;
        self.jobs.retain(|j| {
            if j.kind == JobKind::Index && j.tab == tab {
                self.budget.release(j.mem_budget);
                released = true;
                false
            } else {
                true
            }
        });
        if released {
            self.schedule();
        }
    }

    /// Whether `j` may leave the queue now (slots only, not the budget).
    fn slot_free(&self, j: &JobHandle) -> bool {
        match j.kind {
            JobKind::Sort | JobKind::Export | JobKind::Profile | JobKind::Dupes => !self
                .jobs
                .iter()
                .any(|o| o.id != j.id && o.tab == j.tab && o.kind == j.kind && o.is_active()),
            JobKind::Filter | JobKind::Index => true,
        }
    }

    /// Starts the queued jobs that may start, in FIFO order.
    fn schedule(&mut self) {
        let Some(exec) = self.exec.clone() else {
            return;
        };
        for i in 0..self.jobs.len() {
            if self.jobs[i].state != JobState::Queued || !self.slot_free(&self.jobs[i]) {
                continue;
            }
            let req = BudgetRequest::for_kind(self.jobs[i].kind, self.jobs[i].needed);
            let Some(grant) = self.budget.try_allocate(req) else {
                continue;
            };
            self.start_job(i, grant, exec.clone());
        }
    }

    fn start_job(&mut self, i: usize, grant: u64, exec: Executor) {
        let tmp_dir = self.tmp_dir.clone();
        let msg_tx = self.msg_tx.clone();
        let j = &mut self.jobs[i];
        let Some(start) = j.start.take() else {
            return;
        };
        j.state = JobState::Running;
        j.mem_budget = grant;
        j.started = Some(crate::app::now());
        let ctx = JobContext {
            exec,
            cancel: j.cancel.clone(),
            pause: j.pause.clone(),
            progress: j.progress.clone(),
            budget: grant,
            tmp_dir,
        };
        let id = j.id;
        let inner = tokio::spawn(start(ctx));
        j.abort = Some(inner.abort_handle());
        j.task = Some(tokio::spawn(async move {
            let result = match inner.await {
                Ok(r) => r,
                Err(e) if e.is_cancelled() => Err(JobError::Cancelled),
                Err(e) => Err(JobError::Other(format!("job panicked: {e}"))),
            };
            if let Some(tx) = msg_tx {
                let _ = tx.send(Msg::Job(JobMsg::Finished { id, result }));
            }
        }));
    }

    /// A job's task ended. Updates its state, releases its grant, applies
    /// the retention rule and starts queued jobs. Returns what the app
    /// needs to apply the result; `None` for an unknown job.
    pub fn finished(&mut self, id: JobId, result: Result<JobOutput, JobError>) -> Option<Finished> {
        let i = self.position(id)?;
        let j = &mut self.jobs[i];
        self.budget.release(j.mem_budget);
        j.mem_budget = 0;
        j.task = None;
        j.abort = None;
        let cancelled =
            j.state == JobState::Cancelled || result.as_ref().is_err_and(JobError::is_cancelled);
        let out = Finished {
            id,
            kind: j.kind,
            tab: j.tab,
            view: j.view,
            title: j.title.clone(),
            result: (!cancelled).then_some(result),
        };
        if cancelled {
            self.jobs.remove(i);
        } else {
            j.state = match &out.result {
                Some(Err(e)) => e.to_state(),
                _ => JobState::Done,
            };
            j.finished = Some(crate::app::now());
            self.retain_finished();
        }
        self.schedule();
        Some(out)
    }

    /// Drops the oldest finished jobs beyond [`KEEP_FINISHED`].
    fn retain_finished(&mut self) {
        let mut finished: Vec<(Instant, JobId)> = self
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Done | JobState::Failed(_)))
            .map(|j| (j.finished.unwrap_or_else(crate::app::now), j.id))
            .collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort();
        let drop: Vec<JobId> = finished[..finished.len() - KEEP_FINISHED]
            .iter()
            .map(|(_, id)| *id)
            .collect();
        self.jobs.retain(|j| !drop.contains(&j.id));
    }

    /// Cancels a job (`K`, a superseding filter, a popped view). A queued
    /// job is removed at once; a running or paused one is woken, hidden and
    /// removed when its task ends. Finished and index jobs are left alone.
    /// Returns whether something was cancelled.
    pub fn cancel(&mut self, id: JobId) -> bool {
        let Some(i) = self.position(id) else {
            return false;
        };
        let j = &mut self.jobs[i];
        match j.state {
            JobState::Queued => {
                j.cancel.cancel();
                self.jobs.remove(i);
                true
            }
            JobState::Running | JobState::Paused if j.kind != JobKind::Index => {
                j.cancel.cancel();
                // Cancelling must wake paused workers (M5-01).
                j.pause.resume();
                j.state = JobState::Cancelled;
                true
            }
            _ => false,
        }
    }

    /// `p`: pauses a running job or resumes a paused one. Index jobs can't
    /// be paused (the indexer has no pause point). Returns whether the
    /// state changed.
    pub fn toggle_pause(&mut self, id: JobId) -> bool {
        let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) else {
            return false;
        };
        if j.kind == JobKind::Index {
            return false;
        }
        match j.state {
            JobState::Running => {
                j.pause.pause();
                j.state = JobState::Paused;
                true
            }
            JobState::Paused => {
                j.pause.resume();
                j.state = JobState::Running;
                true
            }
            _ => false,
        }
    }

    /// `d`: removes a finished job's line. No effect on other jobs.
    pub fn dismiss(&mut self, id: JobId) -> bool {
        let before = self.jobs.len();
        self.jobs
            .retain(|j| !(j.id == id && matches!(j.state, JobState::Done | JobState::Failed(_))));
        self.jobs.len() != before
    }

    /// Cancels every pending job of `tab` (except exports when
    /// `keep_exports`) and ends its index job. For a closed tab, a reload
    /// and a dialect change.
    pub fn cancel_tab(&mut self, tab: TabId, keep_exports: bool) {
        let ids: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|j| {
                j.tab == tab
                    && j.kind != JobKind::Index
                    && j.is_pending()
                    && !(keep_exports && j.kind == JobKind::Export)
            })
            .map(|j| j.id)
            .collect();
        for id in ids {
            self.cancel(id);
        }
        self.end_index(tab);
    }

    /// Cancels the pending filters and sorts of `tab` that feed or read a
    /// view from `from` up: those views are being popped (M4-03). Exports
    /// may finish (they only write a file).
    pub fn cancel_views(&mut self, tab: TabId, from: ViewKey) {
        let ids: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|j| {
                j.tab == tab
                    && matches!(j.kind, JobKind::Filter | JobKind::Sort)
                    && j.is_pending()
                    && (j.view.is_some_and(|v| v >= from) || j.parent.is_some_and(|p| p >= from))
            })
            .map(|j| j.id)
            .collect();
        for id in ids {
            self.cancel(id);
        }
    }

    /// Filter, Sort, Profile and Export jobs that are running, paused or
    /// queued, in `tab` or everywhere: what `q` and `Ctrl-w` confirm
    /// (indexing is excluded: losing it costs nothing).
    pub fn pending_count(&self, tab: Option<TabId>) -> usize {
        self.jobs
            .iter()
            .filter(|j| j.kind != JobKind::Index && j.is_pending())
            .filter(|j| tab.is_none_or(|t| j.tab == t))
            .count()
    }

    /// Jobs (indexing excluded) running or paused: the status line's
    /// `N jobs`.
    pub fn running_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| j.kind != JobKind::Index && j.is_active())
            .count()
    }

    /// Whether the 100 ms tick must run for progress (M0-02).
    pub fn has_running(&self) -> bool {
        self.jobs.iter().any(|j| j.state == JobState::Running)
    }

    /// The drawer's lines (M5-02): running and paused first (oldest
    /// first), then queued (FIFO), then finished (newest first). Cancelled
    /// jobs are not listed.
    pub fn drawer_order(&self) -> Vec<&JobHandle> {
        let listed = || self.jobs.iter().filter(|j| j.is_listed());
        let mut out: Vec<&JobHandle> = listed().filter(|j| j.is_active()).collect();
        out.sort_by_key(|j| (j.started, j.id));
        out.extend(listed().filter(|j| j.state == JobState::Queued));
        let mut done: Vec<&JobHandle> = listed().filter(|j| j.state.is_finished()).collect();
        done.sort_by_key(|j| std::cmp::Reverse((j.finished, j.id)));
        out.extend(done);
        out
    }

    /// Number of drawer lines.
    pub fn drawer_len(&self) -> usize {
        self.jobs.iter().filter(|j| j.is_listed()).count()
    }

    /// Mutable access for the app's per-frame progress sync (index jobs).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut JobHandle> {
        self.jobs.iter_mut()
    }

    /// Quitting (M5-01): cancels every job, then waits up to `timeout` for
    /// their tasks, so the RAII temp files are dropped; whatever is still
    /// running after that is aborted.
    pub async fn shutdown(&mut self, timeout: Duration) {
        let mut tasks = Vec::new();
        for j in &mut self.jobs {
            j.cancel.cancel();
            j.pause.resume();
            if let Some(t) = j.task.take() {
                tasks.push((t, j.abort.take()));
            }
        }
        let aborts: Vec<AbortHandle> = tasks
            .iter()
            .filter_map(|(t, a)| a.clone().map(|a| (a, t.abort_handle())))
            .flat_map(|(a, b)| [a, b])
            .collect();
        let all = futures::future::join_all(tasks.into_iter().map(|(t, _)| t));
        if tokio::time::timeout(timeout, all).await.is_err() {
            for a in aborts {
                a.abort();
            }
        }
        self.jobs.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use pretty_assertions::assert_eq;
    use tachy_core::jobs::{MIB, SORT_MIN};
    use tokio::sync::mpsc;

    use super::*;

    /// A fake job: loops on `check()`, counting progress, until `stop` is
    /// set (then succeeds) or it is cancelled.
    fn looping(stop: Arc<AtomicBool>) -> StartFn {
        Box::new(move |ctx: JobContext| {
            Box::pin(async move {
                let ctl = ctx.control();
                let progress = ctx.progress.clone();
                tokio::task::spawn_blocking(move || {
                    loop {
                        ctl.check()?;
                        if let Progress::Rows(p) = &progress {
                            p.add(1);
                        }
                        if stop.load(Ordering::Relaxed) {
                            return Ok(JobOutput::FilterDone);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
                .await
                .unwrap()
            }) as JobFuture
        })
    }

    fn spec(kind: JobKind, tab: u64, parent: Option<ViewKey>, start: StartFn) -> JobSpec {
        JobSpec {
            kind,
            tab: TabId(tab),
            view: parent.map(|p| p + 1),
            parent,
            title: format!("{kind:?}"),
            progress: Progress::for_kind(JobKind::Profile, 0),
            needed: SORT_MIN,
            cancel: CancellationToken::new(),
            start,
        }
    }

    struct Harness {
        jobs: JobManager,
        rx: mpsc::UnboundedReceiver<Msg>,
        exec: Executor,
    }

    impl Harness {
        fn new(mem: u64) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            let mut jobs = JobManager::new(mem, std::env::temp_dir());
            jobs.set_sender(tx);
            Harness {
                jobs,
                rx,
                exec: Executor::new(4),
            }
        }

        fn submit(&mut self, s: JobSpec) -> JobId {
            self.jobs.submit(s, self.exec.clone())
        }

        /// Waits for the next Finished message and feeds it back.
        async fn next_finished(&mut self) -> Finished {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.rx.recv())
                .await
                .expect("no job finished")
                .unwrap();
            let Msg::Job(JobMsg::Finished { id, result }) = msg else {
                panic!("unexpected message");
            };
            self.jobs.finished(id, result).unwrap()
        }

        fn state(&self, id: JobId) -> Option<JobState> {
            self.jobs.get(id).map(|j| j.state.clone())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_sort_on_the_same_tab_queues_until_the_first_ends() {
        let mut h = Harness::new(2048 * MIB);
        let stop1 = Arc::new(AtomicBool::new(false));
        let a = h.submit(spec(JobKind::Sort, 1, None, looping(Arc::clone(&stop1))));
        let b = h.submit(spec(
            JobKind::Sort,
            1,
            None,
            looping(Arc::new(AtomicBool::new(true))),
        ));
        // Another tab has its own slot.
        let c = h.submit(spec(
            JobKind::Sort,
            2,
            None,
            looping(Arc::new(AtomicBool::new(false))),
        ));
        assert_eq!(h.state(a), Some(JobState::Running));
        assert_eq!(h.state(b), Some(JobState::Queued));
        assert_eq!(h.state(c), Some(JobState::Running));
        stop1.store(true, Ordering::Relaxed);
        let f = h.next_finished().await;
        assert_eq!(f.id, a);
        assert_eq!(h.state(a), Some(JobState::Done));
        assert_eq!(h.state(b), Some(JobState::Running));
        let f = h.next_finished().await;
        assert_eq!(f.id, b);
        h.jobs.cancel(c);
        assert_eq!(h.next_finished().await.result.map(|_| ()), None);
        assert!(h.jobs.get(c).is_none(), "cancelled jobs are removed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_filter_cancels_only_the_filter_on_the_same_view() {
        let mut h = Harness::new(2048 * MIB);
        let never = || looping(Arc::new(AtomicBool::new(false)));
        let same = h.submit(spec(JobKind::Filter, 1, Some(0), never()));
        let other_view = h.submit(spec(JobKind::Filter, 1, Some(1), never()));
        let other_tab = h.submit(spec(JobKind::Filter, 2, Some(0), never()));
        let newer = h.submit(spec(JobKind::Filter, 1, Some(0), never()));
        assert_eq!(h.state(same), Some(JobState::Cancelled));
        assert!(!h.jobs.drawer_order().iter().any(|j| j.id == same));
        for id in [other_view, other_tab, newer] {
            assert_eq!(h.state(id), Some(JobState::Running));
        }
        let f = h.next_finished().await;
        assert_eq!(f.id, same);
        assert!(f.result.is_none());
        assert!(h.jobs.get(same).is_none());
        h.jobs.shutdown(SHUTDOWN_TIMEOUT).await;
        assert_eq!(h.jobs.drawer_len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_stops_progress_and_resume_continues() {
        let mut h = Harness::new(2048 * MIB);
        let stop = Arc::new(AtomicBool::new(false));
        let id = h.submit(spec(JobKind::Profile, 1, None, looping(Arc::clone(&stop))));
        let progress = h.jobs.get(id).unwrap().progress.clone();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(h.jobs.toggle_pause(id));
        assert_eq!(h.state(id), Some(JobState::Paused));
        // Let the worker park, then check that the counter stands still.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let parked = progress.done();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(progress.done(), parked);
        assert!(h.jobs.toggle_pause(id));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(progress.done() > parked);
        stop.store(true, Ordering::Relaxed);
        assert!(h.next_finished().await.result.unwrap().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_paused_job_ends_it_quickly() {
        let mut h = Harness::new(2048 * MIB);
        let id = h.submit(spec(
            JobKind::Profile,
            1,
            None,
            looping(Arc::new(AtomicBool::new(false))),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        h.jobs.toggle_pause(id);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let t = std::time::Instant::now();
        h.jobs.cancel(id);
        let f = h.next_finished().await;
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(f.id, id);
        assert!(h.jobs.get(id).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_deletes_the_job_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut jobs = JobManager::new(2048 * MIB, dir.path().to_path_buf());
        jobs.set_sender(tx);
        let start: StartFn = Box::new(|ctx: JobContext| {
            Box::pin(async move {
                let tmp = ctx.temp_dir().map_err(|e| JobError::io("tmp", e))?;
                std::fs::write(tmp.path().join("run-0"), b"x").unwrap();
                ctx.cancel.cancelled().await;
                drop(tmp);
                Err(JobError::Cancelled)
            }) as JobFuture
        });
        let id = jobs.submit(spec(JobKind::Sort, 1, None, start), Executor::new(2));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listing = || std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(listing(), 1);
        jobs.cancel(id);
        let Some(Msg::Job(JobMsg::Finished { id, result })) = rx.recv().await else {
            panic!()
        };
        jobs.finished(id, result);
        assert_eq!(listing(), 0, "no tachy-job-* left");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_eleventh_finished_job_evicts_the_oldest() {
        let mut h = Harness::new(2048 * MIB);
        let mut ids = Vec::new();
        for _ in 0..11 {
            let id = h.submit(spec(
                JobKind::Export,
                1,
                None,
                looping(Arc::new(AtomicBool::new(true))),
            ));
            ids.push(id);
            h.next_finished().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(h.jobs.drawer_len(), KEEP_FINISHED);
        assert!(h.jobs.get(ids[0]).is_none());
        assert!(h.jobs.get(ids[1]).is_some());
        // Newest first.
        assert_eq!(h.jobs.drawer_order()[0].id, ids[10]);
        // `d` dismisses a finished line.
        assert!(h.jobs.dismiss(ids[5]));
        assert_eq!(h.jobs.drawer_len(), KEEP_FINISHED - 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failures_and_panics_are_failed_jobs() {
        let mut h = Harness::new(2048 * MIB);
        let fail: StartFn =
            Box::new(|_| Box::pin(async { Err(JobError::Other("disk full".into())) }) as JobFuture);
        let id = h.submit(spec(JobKind::Export, 1, None, fail));
        h.next_finished().await;
        assert_eq!(h.state(id), Some(JobState::Failed("disk full".into())));
        fn explode() -> Result<JobOutput, JobError> {
            panic!("boom")
        }
        let boom: StartFn = Box::new(|_| Box::pin(async { explode() }) as JobFuture);
        let id = h.submit(spec(JobKind::Export, 1, None, boom));
        h.next_finished().await;
        assert!(matches!(h.state(id), Some(JobState::Failed(m)) if m.contains("panicked")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jobs_queue_until_the_budget_frees() {
        // 128 MiB reserved + 64 MiB free: one filter fits, the second queues.
        let mut h = Harness::new(192 * MIB);
        let stop = Arc::new(AtomicBool::new(false));
        let a = h.submit(spec(
            JobKind::Filter,
            1,
            Some(0),
            looping(Arc::clone(&stop)),
        ));
        let b = h.submit(spec(
            JobKind::Filter,
            2,
            Some(0),
            looping(Arc::new(AtomicBool::new(true))),
        ));
        assert_eq!(h.state(a), Some(JobState::Running));
        assert_eq!(h.state(b), Some(JobState::Queued));
        assert_eq!(h.jobs.budget().free(), 0);
        stop.store(true, Ordering::Relaxed);
        h.next_finished().await;
        assert_eq!(h.state(b), Some(JobState::Running));
        h.next_finished().await;
        assert_eq!(h.jobs.budget().in_use(), 0);
    }

    #[test]
    fn sort_grants_and_index_grants() {
        // A sort takes 75 % of what's free, capped by what it needs.
        let mut b = MemoryBudget::new(2048 * MIB);
        let free = b.free();
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: u64::MAX }),
            Some(free * 3 / 4)
        );
        assert_eq!(
            b.grant_for(BudgetRequest::Sort { needed: 100 * MIB }),
            Some(100 * MIB)
        );
        // An index takes a quarter and never queues.
        let idx = b.try_allocate(BudgetRequest::Index).unwrap();
        assert_eq!(idx, free / 4);
        b.release(idx);
        let mut tiny = MemoryBudget::new(64 * MIB);
        assert_eq!(tiny.try_allocate(BudgetRequest::Index), Some(0));
        assert_eq!(tiny.try_allocate(BudgetRequest::Profile), None);
    }

    #[tokio::test]
    async fn drawer_order_and_counts() {
        let mut h = Harness::new(2048 * MIB);
        let stop = Arc::new(AtomicBool::new(false));
        let idx = h.jobs.register_index(
            TabId(1),
            "a.csv".into(),
            Progress::for_kind(JobKind::Index, 10),
            CancellationToken::new(),
        );
        let s1 = h.submit(spec(JobKind::Sort, 1, None, looping(Arc::clone(&stop))));
        let s2 = h.submit(spec(JobKind::Sort, 1, None, looping(Arc::clone(&stop))));
        let order: Vec<JobId> = h.jobs.drawer_order().iter().map(|j| j.id).collect();
        assert_eq!(order, [idx, s1, s2]);
        assert_eq!(h.jobs.pending_count(None), 2);
        assert_eq!(h.jobs.pending_count(Some(TabId(2))), 0);
        assert_eq!(h.jobs.running_count(), 1);
        h.jobs.end_index(TabId(1));
        assert!(h.jobs.get(idx).is_none());
        // Cancelling a queued job removes it at once.
        assert!(h.jobs.cancel(s2));
        assert!(h.jobs.get(s2).is_none());
        stop.store(true, Ordering::Relaxed);
        h.jobs.shutdown(SHUTDOWN_TIMEOUT).await;
    }
}
