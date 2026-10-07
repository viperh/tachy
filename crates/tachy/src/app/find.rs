//! `App` handlers for the filter bar (M4-04, UI side) and incremental search
//! (M4-05). A child module of `app`, so it shares `App`'s private fields.
//!
//! - `f` / `F` / `/` open the bar (`Mode::Filter` / `Mode::Search`). Keys
//!   not bound in the `Filter` / `Search` contexts edit it
//!   ([`App::bar_key`]); every edit schedules a validation 100 ms later
//!   (run on the tick, [`App::find_tick`]).
//! - `Enter` validates at once. A valid filter goes to
//!   [`App::submit_filter`]; a valid search becomes the active search and
//!   runs from the cursor.
//! - Search is single-flight: one request at a time, the previous one is
//!   cancelled (and awaited) first; `Msg::SearchDone` with an old id is
//!   dropped.

use std::sync::{Arc, atomic::Ordering};

use tachy_core::{
    query::Predicate,
    search::{Direction, SearchOutcome, SearchQuery, SearchRequest, SearchStatus, run_search},
};
use tracing::debug;

use super::{App, now};
use crate::{
    action::Action,
    mode::Mode,
    msg::Msg,
    search_ui::{
        ActiveSearch, BarKind, QueryInput, SearchInflight, VALIDATE_DEBOUNCE, Validation,
        compile_filter, complete, validate,
    },
    state::Focus,
    tab::{Tab, TabId},
    toast::{Toast, ToastLevel},
};

impl App {
    /// Filter and search actions. Returns whether `action` was one (and so
    /// was handled here).
    pub(super) fn find_action(&mut self, action: &Action) -> bool {
        if self.state.overlay.is_some() || self.state.prompt.is_some() {
            return false;
        }
        if self.state.mode == Mode::Filter && self.save_view_action(action) {
            return true;
        }
        match self.state.mode {
            Mode::Filter | Mode::Search => match action {
                Action::Submit => self.bar_submit(),
                Action::Cancel => self.close_bar(),
                Action::HistoryPrev => self.bar_history(true),
                Action::HistoryNext => self.bar_history(false),
                Action::Complete => self.bar_complete(),
                Action::SaveView => self.start_save_view(),
                _ => return false,
            },
            Mode::Normal => match action {
                Action::Filter => self.open_bar(BarKind::Filter, String::new()),
                Action::RefineFilter => {
                    let expr = self
                        .state
                        .active_tab()
                        .and_then(|t| t.views.active_view().filter_expr())
                        .unwrap_or_default()
                        .to_owned();
                    self.open_bar(BarKind::Filter, expr);
                }
                Action::Search => self.open_bar(BarKind::Search, String::new()),
                Action::SearchNext => self.search_again(Direction::Forward),
                Action::SearchPrev => self.search_again(Direction::Backward),
                _ => return false,
            },
            Mode::Command | Mode::Dialog => return false,
        }
        true
    }

    /// Opens the bar on the active (loaded) tab with `text`, the cursor at
    /// its end.
    fn open_bar(&mut self, kind: BarKind, text: String) {
        let Some(tab) = self.state.active_tab().filter(|t| t.loaded.is_some()) else {
            return;
        };
        let mut bar = QueryInput::new(kind, tab.id, text);
        bar.validation = validate(kind, bar.edit.text(), tab);
        self.state.find.history_mut(kind).reset();
        self.state.find.bar = Some(bar);
        self.state.focus = Focus::Table;
        self.state.mode = match kind {
            BarKind::Filter => Mode::Filter,
            BarKind::Search => Mode::Search,
        };
    }

    /// `Esc` in the bar, or after a submit: back to Normal.
    fn close_bar(&mut self) {
        self.state.find.save = None;
        if let Some(bar) = self.state.find.bar.take() {
            self.state.find.history_mut(bar.kind).reset();
        }
        self.state.mode = Mode::Normal;
    }

    /// The tab the open bar belongs to.
    fn bar_tab(&self) -> Option<&Tab> {
        let id = self.state.find.bar.as_ref()?.tab;
        self.state.tabs.iter().find(|t| t.id == id)
    }

    /// A key not bound in the `Filter` / `Search` context: an edit.
    pub(super) fn bar_key(&mut self, key: crossterm::event::KeyEvent) {
        if self.save_view_key(key) {
            return;
        }
        let Some(bar) = self.state.find.bar.as_mut() else {
            return;
        };
        let before = bar.edit.text().to_owned();
        if bar.edit.handle_key(key) && bar.edit.text() != before {
            self.bar_edited();
        }
    }

    /// Bracketed paste into the bar.
    pub(super) fn bar_paste(&mut self, text: &str) {
        if let Some(flow) = self.state.find.save.as_mut() {
            match flow.step {
                crate::search_ui::SaveStep::Name => flow.name.paste(text),
                crate::search_ui::SaveStep::Glob => flow.glob.paste(text),
                _ => {}
            }
            return;
        }
        if let Some(bar) = self.state.find.bar.as_mut() {
            bar.edit.paste(text);
            self.bar_edited();
        }
    }

    /// The text changed: validate after the debounce; the hint shows
    /// meanwhile.
    fn bar_edited(&mut self) {
        if let Some(bar) = self.state.find.bar.as_mut() {
            bar.validation = Validation::Hint;
            bar.validate_at = Some(now() + VALIDATE_DEBOUNCE);
        }
    }

    /// Runs a validation whose debounce has passed, and turns the search
    /// spinner (each tick).
    pub(super) fn find_tick(&mut self, t: std::time::Instant) {
        if let Some(inflight) = self.state.find.search.inflight.as_mut() {
            inflight.frame += 1;
        }
        let due = self
            .state
            .find
            .bar
            .as_ref()
            .and_then(|b| b.validate_at)
            .is_some_and(|at| at <= t);
        if due {
            self.validate_bar();
        }
    }

    /// Validates the bar's text now.
    fn validate_bar(&mut self) {
        let validation = match (self.bar_tab(), self.state.find.bar.as_ref()) {
            (Some(tab), Some(bar)) => validate(bar.kind, bar.edit.text(), tab),
            _ => Validation::Hint,
        };
        if let Some(bar) = self.state.find.bar.as_mut() {
            bar.validation = validation;
            bar.validate_at = None;
        }
    }

    /// `↑` (`prev`) / `↓` in the bar.
    fn bar_history(&mut self, prev: bool) {
        let find = &mut self.state.find;
        let Some(bar) = find.bar.as_mut() else {
            return;
        };
        let history = match bar.kind {
            BarKind::Filter => &mut find.filter_history,
            BarKind::Search => &mut find.search_history,
        };
        let entry = if prev {
            history.prev(bar.edit.text())
        } else {
            history.next()
        };
        if let Some(text) = entry {
            bar.edit.set(text);
            bar.completion = None;
            self.bar_edited();
        }
    }

    /// `Tab` in the bar.
    fn bar_complete(&mut self) {
        let Some(id) = self.state.find.bar.as_ref().map(|b| b.tab) else {
            return;
        };
        let state = &mut self.state;
        let columns = state
            .tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.loaded.as_ref())
            .map(|l| &l.columns[..])
            .unwrap_or_default();
        let Some(bar) = state.find.bar.as_mut() else {
            return;
        };
        if complete(bar, columns) {
            self.bar_edited();
        }
    }

    /// `Enter` in the bar. Empty input does nothing; an invalid one stays
    /// open with the error.
    fn bar_submit(&mut self) {
        let Some(bar) = self.state.find.bar.as_ref() else {
            return;
        };
        let (kind, text) = (bar.kind, bar.edit.text().to_owned());
        if text.trim().is_empty() {
            return;
        }
        let Some(tab) = self.bar_tab() else {
            self.close_bar();
            return;
        };
        match kind {
            BarKind::Filter => match compile_filter(&text, tab) {
                Ok(pred) => self.submit_filter(text, pred),
                Err(e) => self.bar_error(e.message, Some(e.span)),
            },
            BarKind::Search => {
                let columns = tab.loaded.as_ref().map_or(&[][..], |l| &l.columns[..]);
                match tachy_core::search::parse_search(&text, columns) {
                    Ok(query) => {
                        let id = tab.id;
                        self.submit_search(id, query);
                    }
                    Err(message) => self.bar_error(message, None),
                }
            }
        }
    }

    fn bar_error(&mut self, message: String, span: Option<std::ops::Range<usize>>) {
        if let Some(bar) = self.state.find.bar.as_mut() {
            bar.validation = Validation::Invalid { message, span };
            bar.validate_at = None;
        }
    }

    /// A valid filter was submitted (`Enter`, or `--filter` at startup): adds
    /// it to the history and returns to Normal mode.
    pub fn submit_filter(&mut self, expr: String, pred: Predicate) {
        self.state.find.filter_history.push(&expr);
        self.close_bar();
        self.start_filter(expr.clone(), pred);
        debug!(expr, "filter submitted");
    }

    /// A valid search was submitted: it becomes the active search and runs
    /// forward from the cursor.
    fn submit_search(&mut self, tab: TabId, query: SearchQuery) {
        self.state.find.search_history.push(&query.text);
        self.close_bar();
        self.state.find.search.active = Some(ActiveSearch {
            tab,
            query: Arc::new(query),
        });
        self.start_search(Direction::Forward);
    }

    /// `n` / `N`: the active search again, from the cursor. Nothing when
    /// no search is active on this tab.
    fn search_again(&mut self, direction: Direction) {
        let active_tab = self.state.active_tab().map(|t| t.id);
        let active = self.state.find.search.active.as_ref();
        if active.is_some_and(|a| Some(a.tab) == active_tab) {
            self.start_search(direction);
        }
    }

    /// Starts one search request for the active search from the cursor
    /// cell, cancelling the previous request. The new task waits for the
    /// previous one to end before running, so two searches never run at
    /// once.
    fn start_search(&mut self, direction: Direction) {
        let exec = self.executor();
        let Some(active) = self.state.find.search.active.clone() else {
            return;
        };
        let Some(tab) = self.state.tabs.iter().find(|t| t.id == active.tab) else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let display = tab.layout.display();
        let cursor_col = tab.cursor_source_col().unwrap_or(0);
        let req = SearchRequest::new(
            Arc::clone(&active.query),
            &l.columns,
            &display,
            tab.cursor_row,
            cursor_col,
            direction,
        );
        let view = tab.views.active_view().clone();
        let (src, index) = (Arc::clone(&l.source), Arc::clone(&l.index));
        let cancel = tab.cancel.child_token();

        let search = &mut self.state.find.search;
        search.cancel();
        search.last_request += 1;
        let id = search.last_request;
        let status = Arc::new(SearchStatus::default());
        search.inflight = Some(SearchInflight {
            id,
            tab: active.tab,
            cancel: cancel.clone(),
            status: Arc::clone(&status),
            frame: 0,
        });
        let previous = search.last_task.take();
        let (running, overlapped) = (Arc::clone(&search.running), Arc::clone(&search.overlapped));
        let tx = self.msg_tx.clone();
        let tab_id = active.tab;
        search.last_task = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if cancel.is_cancelled() {
                return;
            }
            if running.fetch_add(1, Ordering::SeqCst) > 0 {
                overlapped.store(true, Ordering::SeqCst);
            }
            let result = run_search(req, view, src, index, exec, cancel, status).await;
            running.fetch_sub(1, Ordering::SeqCst);
            let result = match result {
                Ok(outcome) => Ok(outcome),
                Err(tachy_core::jobs::JobError::Cancelled) => return,
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(Msg::SearchDone {
                tab: tab_id,
                request: id,
                result,
            });
        }));
    }

    /// `Msg::SearchDone`: moves the cursor to the hit (row and column), or
    /// says `no match`. A stale request id is dropped.
    pub(super) fn search_done(
        &mut self,
        tab: TabId,
        request: u64,
        result: Result<SearchOutcome, String>,
    ) {
        let search = &mut self.state.find.search;
        if search
            .inflight
            .as_ref()
            .is_none_or(|i| i.id != request || i.tab != tab)
        {
            return;
        }
        search.inflight = None;
        let text = search
            .active
            .as_ref()
            .map_or_else(String::new, |a| a.query.text.clone());
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                let toast = Toast::new(ToastLevel::Error, format!("search failed: {e}"));
                self.state.toasts.push(toast, now());
                return;
            }
        };
        let Some(hit) = outcome.hit else {
            let toast = Toast::new(ToastLevel::Info, format!("no match for \"{text}\""));
            self.state.toasts.push(toast, now());
            return;
        };
        let viewport = self.viewport();
        if let Some(t) = self.state.tabs.iter_mut().find(|t| t.id == tab) {
            t.pending_jump = None;
            t.goto_row(hit.pos + 1, viewport);
            // A `col:` search on a hidden column keeps the column cursor.
            let _ = t.goto_column(hit.col, viewport);
        }
        if outcome.wrapped {
            let toast = Toast::new(ToastLevel::Info, "search wrapped");
            self.state.toasts.push(toast, now());
        }
    }

    /// Cancels a search in flight on `tab` and drops its highlight: the
    /// tab's columns or rows changed (a dialect change, M2-04 / M7-03).
    pub(super) fn forget_search(&mut self, tab: TabId) {
        let search = &mut self.state.find.search;
        if search.inflight.as_ref().is_some_and(|i| i.tab == tab) {
            search.cancel();
        }
        if search.active.as_ref().is_some_and(|a| a.tab == tab) {
            search.active = None;
        }
    }

    /// `Esc` in Normal mode, after toasts and pending jumps: cancels a
    /// search in flight and clears the search highlight. Returns whether
    /// there was anything to clear.
    pub(super) fn dismiss_search(&mut self) -> bool {
        let search = &mut self.state.find.search;
        let running = search.running_request().is_some();
        search.cancel();
        search.active.take().is_some() || running
    }

    /// `--filter <EXPR>` (§3): applied once to the first tab when its
    /// dialect is confirmed (at once with `-y` or an explicit dialect, after
    /// the Detected format dialog closes otherwise) and its first sample is
    /// in (column types). A parse error is an error toast; the file opens
    /// unfiltered.
    pub(super) fn maybe_apply_startup_filter(&mut self) {
        if self.state.find.startup_filter_done {
            return;
        }
        let Some(expr) = self.state.settings.filter.clone() else {
            self.state.find.startup_filter_done = true;
            return;
        };
        if self.state.overlay.is_some() || self.state.prompt.is_some() {
            return;
        }
        let Some(tab) = self.state.tabs.first() else {
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let confirmed = tab.dialog_shown || !self.startup_dialog_expected();
        if !confirmed || (l.sample.is_none() && l.index_error.is_none()) {
            return;
        }
        let compiled = compile_filter(&expr, tab);
        self.state.find.startup_filter_done = true;
        match compiled {
            Ok(pred) => {
                self.state.active_tab = 0;
                self.submit_filter(expr, pred);
            }
            Err(e) => {
                let toast = Toast::new(ToastLevel::Error, format!("--filter: {}", e.message));
                self.state.toasts.push(toast, now());
            }
        }
    }

    /// Whether the Detected format dialog will show before the file is
    /// usable (no `-y`, no explicit dialect, `sniff.confirm` on).
    fn startup_dialog_expected(&self) -> bool {
        let s = &self.state.settings;
        !s.yes && s.sniff_confirm && s.dialect_overrides.is_empty()
    }
}
