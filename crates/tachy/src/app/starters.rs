//! `App` job starters: filter (M4-04), sort (M5-03), profile (M5-04) and
//! export (M6-01). Each builds a [`JobSpec`] whose start closure runs the
//! core job on the app's executor, and submits it to the `JobManager`
//! (M5-01), which applies the concurrency rules and the memory budget. A
//! child module of `app`, so it shares `App`'s private fields.

use std::sync::Arc;

use tachy_core::{
    export::{ExportColumn, ExportContext, ExportOptions, run_export},
    filter::{FilterOutput, ParentRows, run_filter},
    index::IndexPath,
    jobs::{JobError, JobKind, Progress},
    query::Predicate,
    size::format_count,
    sort::{SortJob, SortOptions, SortSpec, estimate, run_sort},
    stats::profile::{ProfileColumn, ProfileOptions, run_profile},
    view::{OrderedKind, View},
};

use super::{App, now};
use crate::{
    commands::ProfileTarget,
    components::dialogs::export::ExportRequest,
    jobs::{JobOutput, JobSpec},
    search_ui::compile_filter,
    toast::{Toast, ToastLevel},
};

/// A job's progress counters didn't match its kind (a bug).
fn wrong_progress() -> JobError {
    JobError::Other("internal error: wrong progress counters for the job".into())
}

/// Views with more rows than this get the sort cost estimate as a toast
/// when a sort starts from `s` / `S` (M5-03).
const SORT_ESTIMATE_TOAST_ROWS: u64 = 10_000_000;

impl App {
    fn toast(&mut self, level: ToastLevel, text: impl Into<String>) {
        self.state
            .toasts
            .push(Toast::new(level, text.into()), now());
    }

    // ---- filter (M4-04) ------------------------------------------------

    /// Starts a filter job on the active view of the active tab (§8.2): the
    /// result view is pushed at once and fills while the job runs, in file
    /// order (or in the parent's order for a sorted parent, D10). The
    /// `JobManager` cancels a running filter on the same parent (§10.3).
    pub(super) fn start_filter(&mut self, expr: String, pred: Predicate) {
        let tmp_dir = self.state.settings.tmp_dir.clone();
        let Some(tab) = self.state.active_tab_mut() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let parent_key = tab.views.active();
        let parent_view = tab.views.active_view().clone();
        let parent = ParentRows::from_view(&parent_view);
        let out = match FilterOutput::for_parent(&parent, &tmp_dir) {
            Ok(out) => out,
            Err(e) => {
                self.toast(ToastLevel::Error, format!("filter: {e}"));
                return;
            }
        };
        let columns = pred.columns().to_vec();
        let view = match (out.rows(), out.list()) {
            (Some(rows), _) => View::Filtered {
                rows: Arc::clone(rows),
                expr: expr.clone(),
                columns,
            },
            (_, Some(list)) => View::Ordered {
                list: Arc::clone(list),
                kind: OrderedKind::FilteredSorted {
                    expr: expr.clone(),
                    columns,
                    keys: parent_view.sort_keys().to_vec(),
                },
            },
            _ => return,
        };
        let src = Arc::clone(&l.source);
        let index = Arc::clone(&l.index);
        // The pre-filter needs "every `\n` ends a record" (M4-04).
        let prefilter = l
            .index_summary
            .as_ref()
            .map_or(!l.sniff.quoted_newlines, |s| s.path_used == IndexPath::Fast);
        let total = src.len().saturating_sub(src.data_start());
        let progress = Progress::for_kind(JobKind::Filter, total);
        let tab_id = tab.id;
        let cancel = tab.cancel.child_token();
        let key = tab.push_view(view, None);
        let job_pred = pred.clone();
        let spec = JobSpec {
            kind: JobKind::Filter,
            tab: tab_id,
            view: Some(key),
            parent: Some(parent_key),
            title: format!("filter {expr}"),
            progress,
            needed: 0,
            cancel,
            start: Box::new(move |ctx| {
                Box::pin(async move {
                    let Progress::Filter(fp) = ctx.progress else {
                        return Err(wrong_progress());
                    };
                    run_filter(
                        parent, src, index, job_pred, out, ctx.exec, ctx.cancel, ctx.pause, fp,
                        prefilter,
                    )
                    .await
                    .map(|()| JobOutput::FilterDone)
                })
            }),
        };
        let id = self.submit_job(spec);
        if let Some(tab) = self.state.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.set_view_job(key, Some(id), Some(pred));
        }
    }

    /// `filter <expr>` from the palette: compiles against the active tab,
    /// then starts the filter as if it was typed after `f`.
    pub(super) fn submit_filter_text(&mut self, text: String) {
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        match compile_filter(&text, tab) {
            Ok(pred) => self.submit_filter(text, pred),
            Err(e) => self.toast(ToastLevel::Error, format!("filter: {}", e.message)),
        }
    }

    /// Whether the active view of the active tab is being filled by a
    /// running (or paused) filter job: the query bar shows its gauge, and
    /// `Esc` cancels it (§12.4, §13).
    pub(crate) fn running_filter_job(&self) -> Option<crate::jobs::JobId> {
        self.state.running_filter().map(|j| j.id)
    }

    // ---- sort (M5-03) --------------------------------------------------

    /// `s` / `S`: sorts the active view by the cursor column.
    pub(super) fn sort_cursor_column(&mut self, descending: bool) {
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        let Some(column) = tab.cursor_source_col() else {
            return;
        };
        let keys = vec![tachy_core::sort::SortKey {
            column,
            descending,
            ci: false,
        }];
        let rows = tab.view_len();
        if rows > SORT_ESTIMATE_TOAST_ROWS {
            let free = self.state.jobs.budget().free();
            let text = estimate(rows, &keys, free).text();
            self.toast(ToastLevel::Info, text);
        }
        self.start_sort(SortSpec { keys });
    }

    /// Starts a sort job on the active view (§8.4). The `Ordered { Sorted }`
    /// view is pushed when it finishes (`job_finished`). At most one sort
    /// per file runs; others queue (§10.3).
    pub(super) fn start_sort(&mut self, spec: SortSpec) {
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        if spec.keys.is_empty() {
            return;
        }
        let rows = tab.view_len();
        let job = SortJob {
            src: Arc::clone(&l.source),
            index: Arc::clone(&l.index),
            parent: tab.views.active_view().clone(),
            keys: spec.keys.clone(),
            columns: l.columns.clone(),
            nulls: tab.nulls.clone(),
            ram_cap: 0,
            tmp_dir: self.state.settings.tmp_dir.clone(),
            options: SortOptions::default(),
        };
        let title = format!("sort {}", spec.display(&l.columns));
        let needed = rows.saturating_mul(tachy_core::sort::key::RECORD_BYTES as u64);
        let progress = Progress::for_kind(JobKind::Sort, rows);
        let keys = spec.keys;
        let spec = JobSpec {
            kind: JobKind::Sort,
            tab: tab.id,
            view: None,
            parent: Some(tab.views.active()),
            title,
            progress,
            needed,
            cancel: tab.cancel.child_token(),
            start: Box::new(move |ctx| {
                Box::pin(async move {
                    let Progress::Sort(sp) = ctx.progress.clone() else {
                        return Err(wrong_progress());
                    };
                    let ctl = ctx.control();
                    let job = SortJob {
                        ram_cap: ctx.budget,
                        tmp_dir: ctx.tmp_dir.clone(),
                        ..job
                    };
                    let list = run_sort(job, &ctx.exec, &ctl, sp).await?;
                    Ok(JobOutput::SortDone { list, keys })
                })
            }),
        };
        self.submit_job(spec);
    }

    /// `--sort` once the startup filter (if any) has been applied and the
    /// dialect is confirmed (§3: the filtered view is sorted).
    pub(super) fn maybe_apply_startup_sort(&mut self) {
        if self.startup_sort_done || !self.state.find.startup_filter_done {
            return;
        }
        let Some(text) = self.state.settings.sort.clone() else {
            self.startup_sort_done = true;
            return;
        };
        let Some(tab) = self.state.tabs.first() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        if self.state.overlay.is_some() || l.sample.is_none() {
            return;
        }
        self.startup_sort_done = true;
        match SortSpec::parse(&text, &l.columns) {
            Ok(spec) => {
                self.state.active_tab = 0;
                self.start_sort(spec);
            }
            Err(e) => self.toast(ToastLevel::Error, format!("--sort: {}", e.message)),
        }
    }

    // ---- profile (M5-04) -----------------------------------------------

    /// `profile <col>|all`: a full-file statistics job (§7.2). It waits for
    /// the index, and replaces the sample stats when done.
    pub(super) fn start_profile(&mut self, target: ProfileTarget) {
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let columns: Vec<ProfileColumn> = match target {
            ProfileTarget::All => ProfileColumn::all(&l.columns),
            ProfileTarget::Column(c) => match l.columns.get(c) {
                Some(meta) => vec![ProfileColumn::from_meta(c, meta)],
                None => return,
            },
        };
        let title = match target {
            ProfileTarget::All => format!("profile all ({} cols)", columns.len()),
            ProfileTarget::Column(c) => format!("profile {}", l.columns[c].name.display),
        };
        let src = Arc::clone(&l.source);
        let index = Arc::clone(&l.index);
        let nulls = tab.nulls.clone();
        let total = index.total_rows().unwrap_or(0);
        let progress = Progress::for_kind(JobKind::Profile, total);
        let spec = JobSpec {
            kind: JobKind::Profile,
            tab: tab.id,
            view: None,
            parent: None,
            title,
            progress,
            needed: 0,
            cancel: tab.cancel.child_token(),
            start: Box::new(move |ctx| {
                Box::pin(async move {
                    let Progress::Rows(rp) = ctx.progress.clone() else {
                        return Err(wrong_progress());
                    };
                    let opts = ProfileOptions {
                        budget: ctx.budget,
                        ..ProfileOptions::default()
                    };
                    let ctl = ctx.control();
                    let result =
                        run_profile(src, index, columns, nulls, ctx.exec, ctl, rp, opts).await?;
                    Ok(JobOutput::ProfileDone(result))
                })
            }),
        };
        self.submit_job(spec);
    }

    // ---- export (M6-01) ------------------------------------------------

    /// Starts the export of the active tab's current view described by
    /// `req` (the dialog has closed and validated the target). At most one
    /// export per file runs; others queue (§10.3).
    pub fn submit_export(&mut self, req: ExportRequest) {
        let threads = self.executor().threads();
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let cols =
            ExportColumn::select(&l.columns, &tab.layout.display(), req.columns_visible_only);
        let view = ParentRows::from_view(tab.views.active_view());
        let src = Arc::clone(&l.source);
        let index = Arc::clone(&l.index);
        let rows = tab.view_len();
        let data = src.len().saturating_sub(src.data_start());
        let avg = data / index.indexed_rows().max(1);
        let progress = Progress::for_kind(JobKind::Export, rows);
        let name = req.target.file_name().map_or_else(
            || req.target.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let spec = JobSpec {
            kind: JobKind::Export,
            tab: tab.id,
            view: None,
            parent: Some(tab.views.active()),
            title: format!("export {name}"),
            progress,
            needed: 0,
            cancel: tab.cancel.child_token(),
            start: Box::new(move |ctx| {
                Box::pin(async move {
                    let Progress::Export(ep) = ctx.progress.clone() else {
                        return Err(wrong_progress());
                    };
                    let opts = ExportOptions::for_budget(
                        req.delimiter,
                        req.quoting,
                        req.header,
                        ctx.budget,
                        threads,
                        avg,
                    );
                    let ectx = ExportContext {
                        exec: ctx.exec.clone(),
                        ctl: ctx.control(),
                        progress: ep,
                    };
                    let summary =
                        run_export(view, src, index, cols, opts, req.target, ectx).await?;
                    Ok(JobOutput::ExportDone {
                        path: summary.path,
                        rows: summary.rows,
                    })
                })
            }),
        };
        self.submit_job(spec);
    }

    /// Info toast for a finished export (§8.5): `exported N rows to <name>`.
    pub(super) fn export_done_text(path: &std::path::Path, rows: u64) -> String {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        format!("exported {} rows to {name}", format_count(rows))
    }
}
