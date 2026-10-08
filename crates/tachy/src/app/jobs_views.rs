//! `App` handlers for jobs (M5-01, M5-02), the view stack (M4-03), the
//! clipboard (M6-05), file-change detection and reload (M7-03). A child
//! module of `app`, so it shares `App`'s private fields.

use std::sync::{Arc, atomic::Ordering};

use tachy_core::{
    dupes::DupeRows,
    jobs::{JobError, JobKind, Progress},
    view::{OrderedKind, View},
};

use super::{App, OpenRequest, now};
use crate::{
    action::Action,
    clipboard::{self, Multiplexer},
    components::dialogs::confirm::{ConfirmAction, ConfirmState},
    jobs::{JobId, JobOutput, JobSpec},
    state::DialogKind,
    tab::{PopOutcome, TabId, ViewEntry, Viewport},
    toast::{Toast, ToastLevel},
    watch::{self, FileStamp, WatchRequest},
};

impl App {
    // ---- jobs (M5-01) --------------------------------------------------

    /// Submits a job (filter, sort, profile, export) under the M5-01 rules.
    /// For the tasks that start jobs (M4-04, M5-03, M5-04, M6-01).
    pub fn submit_job(&mut self, spec: JobSpec) -> JobId {
        let exec = self.executor();
        self.state.jobs.submit(spec, exec)
    }

    /// `q`: quits, or asks first when Filter / Sort / Profile / Export jobs
    /// are running, paused or queued. Indexing alone never asks.
    pub(super) fn request_quit(&mut self) {
        let n = self.state.jobs.pending_count(None);
        if n == 0 {
            self.should_quit = true;
        } else {
            self.open_confirm(ConfirmState::jobs_running(n, "quit", ConfirmAction::Quit));
        }
    }

    pub(super) fn open_confirm(&mut self, confirm: ConfirmState) {
        self.state.confirm = Some(confirm);
        self.state.open_dialog(DialogKind::Confirm);
    }

    /// `y` / `Enter` runs the confirmed action, `n` / `Esc` closes.
    pub(super) fn confirm_action(&mut self, action: &Action) {
        match action {
            Action::Submit => {
                let confirm = self.state.confirm.take();
                self.state.close_dialog();
                match confirm.map(|c| c.action) {
                    // `run` cancels every job and waits for them (2 s).
                    Some(ConfirmAction::Quit) => self.should_quit = true,
                    Some(ConfirmAction::CloseTab(id)) => self.close_tab_now(id),
                    Some(ConfirmAction::KillJob(id)) => self.kill_job(id, false),
                    None => {}
                }
            }
            Action::Cancel => {
                self.state.confirm = None;
                self.state.close_dialog();
            }
            _ => {}
        }
    }

    /// `Msg::Job(Finished)`: updates the job list, then applies the result
    /// to its tab (dropped if the tab is gone or the job was cancelled).
    pub(super) fn job_finished(&mut self, id: JobId, result: Result<JobOutput, JobError>) {
        let Some(done) = self.state.jobs.finished(id, result) else {
            return;
        };
        self.clamp_drawer_selection();
        let Some(result) = done.result else {
            return;
        };
        let Some(idx) = self.tab_index(done.tab) else {
            return;
        };
        match result {
            Ok(JobOutput::FilterDone) => {}
            Ok(JobOutput::SortDone { list, keys }) => {
                let tab = &mut self.state.tabs[idx];
                tab.push_view(
                    View::Ordered {
                        list,
                        kind: OrderedKind::Sorted { keys },
                    },
                    None,
                );
            }
            Ok(JobOutput::DupesDone { result, spec, keys }) => {
                let text = result.summary(spec.mode);
                let tab = &mut self.state.tabs[idx];
                let view = match result.rows {
                    DupeRows::Rows(rows) => View::Dupes { rows, spec },
                    DupeRows::List(list) => View::Ordered {
                        list,
                        kind: OrderedKind::Dupes { spec, keys },
                    },
                };
                tab.push_view(view, None);
                self.state
                    .toasts
                    .push(Toast::new(ToastLevel::Info, text), now());
            }
            Ok(JobOutput::ProfileDone(profile)) => {
                if let Some(l) = self.state.tabs[idx].loaded.as_mut() {
                    profile.apply(&mut l.columns);
                }
            }
            Ok(JobOutput::ExportDone { path, rows }) => {
                let text = Self::export_done_text(&path, rows);
                self.state
                    .toasts
                    .push(Toast::new(ToastLevel::Info, text), now());
            }
            Err(e) => {
                let text = format!("{} failed: {e}", done.title);
                self.state
                    .toasts
                    .push(Toast::new(ToastLevel::Error, text), now());
            }
        }
    }

    /// Copies each index job's progress from its tab's index (once per
    /// frame): the indexer publishes `bytes_scanned`, not a job counter.
    pub(super) fn sync_index_progress(&mut self) {
        let tabs = &self.state.tabs;
        for job in self.state.jobs.iter_mut() {
            if job.kind != JobKind::Index {
                continue;
            }
            let (Progress::Bytes(p), Some(l)) = (
                &job.progress,
                tabs.iter()
                    .find(|t| t.id == job.tab)
                    .and_then(|t| t.loaded.as_ref()),
            ) else {
                continue;
            };
            let done = l.index.bytes_scanned().min(p.total());
            p.done.store(done, Ordering::Relaxed);
        }
    }

    // ---- jobs drawer (M5-02) -------------------------------------------

    fn clamp_drawer_selection(&mut self) {
        let n = self.state.jobs.drawer_len();
        self.state.jobs_selected = self.state.jobs_selected.min(n.saturating_sub(1));
    }

    /// `j`/`k` select, `p` pauses or resumes, `K` kills, `d` dismisses.
    pub(super) fn drawer_action(&mut self, action: &Action) {
        self.clamp_drawer_selection();
        let order: Vec<JobId> = self
            .state
            .jobs
            .drawer_order()
            .iter()
            .map(|j| j.id)
            .collect();
        let sel = self.state.jobs_selected;
        let selected = order.get(sel).copied();
        match action {
            Action::SelectNext => {
                self.state.jobs_selected = (sel + 1).min(order.len().saturating_sub(1));
            }
            Action::SelectPrev => self.state.jobs_selected = sel.saturating_sub(1),
            Action::PauseJob => {
                if let Some(id) = selected {
                    self.state.jobs.toggle_pause(id);
                }
            }
            Action::KillJob => {
                if let Some(id) = selected {
                    self.kill_job(id, true);
                }
            }
            Action::DismissJob => {
                if let Some(id) = selected {
                    self.state.jobs.dismiss(id);
                }
            }
            _ => {}
        }
        self.clamp_drawer_selection();
    }

    /// `K` (D5): kills a job. A queued job is just removed. A sort past
    /// 50 % asks first (when `ask`). Killing a filter pops its view (and the
    /// views above it). Index jobs and finished jobs are left alone.
    pub(super) fn kill_job(&mut self, id: JobId, ask: bool) {
        let Some(job) = self.state.jobs.get(id) else {
            return;
        };
        if job.kind == JobKind::Index || job.state.is_finished() {
            return;
        }
        let past_half = job.overall_fraction().is_some_and(|f| f > 0.5);
        if ask && job.kind == JobKind::Sort && job.is_active() && past_half {
            self.open_confirm(ConfirmState {
                text: "kill sort? (y/n)".to_owned(),
                action: ConfirmAction::KillJob(id),
            });
            return;
        }
        let (kind, tab, view) = (job.kind, job.tab, job.view);
        if !self.state.jobs.cancel(id) || kind != JobKind::Filter {
            return;
        }
        let (Some(key), Some(idx)) = (view, self.tab_index(tab)) else {
            return;
        };
        let viewport = self.viewport();
        let t = &mut self.state.tabs[idx];
        if t.views
            .entries()
            .get(key)
            .is_some_and(|e| e.job == Some(id))
        {
            let dropped = t.truncate_views(key, viewport);
            self.cancel_dropped(tab, key, &dropped);
        }
    }

    /// Cancels the jobs of views dropped from `from` up.
    fn cancel_dropped(&mut self, tab: TabId, from: usize, dropped: &[ViewEntry]) {
        for id in dropped.iter().filter_map(|e| e.job) {
            self.state.jobs.cancel(id);
        }
        self.state.jobs.cancel_views(tab, from);
    }

    // ---- views (M4-03) -------------------------------------------------

    /// `x`: pops the top view (cancelling its job, which deletes its temp
    /// files when it ends), or returns from `Enter`'s all-rows view.
    pub(super) fn pop_view(&mut self, viewport: Viewport) {
        let Some(tab) = self.state.active_tab_mut() else {
            return;
        };
        let id = tab.id;
        let key = tab.views.top();
        if let PopOutcome::Popped(entry) = tab.pop_view(viewport) {
            self.cancel_dropped(id, key, &[entry]);
        }
    }

    // ---- clipboard (M6-05) ---------------------------------------------

    /// `y` copies the cursor cell's full value, `Y` the whole record as
    /// stored (every field in source order, `_extraN` included, source
    /// delimiter, minimal quoting). Queued as OSC 52 for after the next
    /// frame. No-op without a tab or in an empty view.
    pub(super) fn copy(&mut self, row: bool) {
        let Some(tab) = self.state.tabs.get_mut(self.state.active_tab) else {
            return;
        };
        let col = tab.cursor_source_col();
        let Some(record) = tab.cursor_record() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let src = &l.source;
        let enc = src.dialect().encoding;
        let value = |i: usize| {
            let mut scratch = Vec::new();
            record
                .value(src, i, &mut scratch)
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        };
        let (text, toast) = if row {
            let fields: Vec<Vec<u8>> = (0..record.field_count()).map(value).collect();
            let text = clipboard::row_text(&fields, src.dialect().delimiter, enc);
            (text, clipboard::copied_row_toast(fields.len()))
        } else {
            let Some(meta) = col.and_then(|c| l.columns.get(c)) else {
                return;
            };
            let text = clipboard::cell_text(&value(meta.source_index), enc);
            let toast = clipboard::copied_cell_toast(&text);
            (text, toast)
        };
        let toast = match clipboard::copy_sequence(&text, Multiplexer::detect()) {
            Ok(seq) => {
                self.pending_osc.extend_from_slice(&seq);
                toast
            }
            Err(e) => e.toast(),
        };
        self.state.toasts.push(toast, now());
    }

    // ---- file changes and reload (M7-03) -------------------------------

    /// Watches tab `idx`'s file for changes (every 2 s) until its token is
    /// cancelled. Not for stdin; a UTF-16 tab watches the original file
    /// (`stamp` was taken on it before transcoding).
    pub(super) fn spawn_watcher(&self, idx: usize, stamp: Option<FileStamp>) {
        let Some(tab) = self.state.tabs.get(idx) else {
            return;
        };
        let Some(initial) = stamp else {
            return;
        };
        if tab.temp.is_some() {
            return;
        }
        let req = WatchRequest {
            tab: tab.id,
            path: tab.path.clone(),
            initial,
            cancel: tab.cancel.child_token(),
        };
        watch::spawn(req, self.msg_tx.clone());
    }

    /// `R` (D7): reloads the active tab from disk with its **current**
    /// dialect (no re-sniff, no dialog; the user can change it afterwards).
    /// Cancels the tab's jobs and indexer, drops every view but `All`,
    /// re-opens the file (a new `Source`, a fresh index and sample, a new
    /// watcher), and puts the cursor back on the same record and column.
    /// Harmless without a pending change. Ignored while the tab is still
    /// opening.
    pub(super) fn reload_active(&mut self) {
        let idx = self.state.active_tab;
        let Some(tab) = self.state.tabs.get(idx) else {
            return;
        };
        let Some(overrides) = tab.reload_overrides() else {
            return;
        };
        let id = tab.id;
        self.state.jobs.cancel_tab(id, false);
        let tab = &mut self.state.tabs[idx];
        let (_dropped, progress) = tab.begin_reload();
        let settings = &self.state.settings;
        let request = OpenRequest {
            path: tab.path.clone(),
            display_name: Some(tab.name.clone()),
            sample_bytes: settings.sniff_sample_bytes,
            overrides,
            tmp_dir: settings.tmp_dir.clone(),
            progress: Arc::clone(&progress),
            cancel: tab.cancel.child_token(),
        };
        let generation = tab.generation;
        self.spawn_open(id, generation, request);
    }

    /// The SIGBUS message names the open file (or `the open file` when
    /// several are open): updated whenever tabs open or close (M7-03).
    pub(super) fn update_sigbus_message(&self) {
        #[cfg(unix)]
        {
            let paths: Vec<&std::path::Path> = self
                .state
                .tabs
                .iter()
                .filter(|t| t.temp.is_none())
                .map(|t| t.path.as_path())
                .collect();
            crate::sigbus::set_message(&paths);
        }
    }
}
