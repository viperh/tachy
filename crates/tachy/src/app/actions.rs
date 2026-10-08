//! `App` handlers for dialogs, panel focus and pending jumps (M2-03, M2-04,
//! M3-03, M3-04). A child module of `app`, so it shares `App`'s private
//! fields.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tachy_core::{size::format_count, types::ColType};

use super::{App, now};
use crate::{
    action::Action,
    commands::EditChange,
    components::{
        dialogs::{
            column_chooser::ColumnChooserState,
            detected_format::DetectedFormatState,
            export::{CompleteStep, ExportForm, ExportInfo, KeyOutcome},
            goto::GotoDialog,
            value_popup::ValuePopupState,
        },
        layout::{INSPECTOR_MIN_TERMINAL_WIDTH, compute_layout},
    },
    goto::{GotoTarget, parse_goto},
    msg::Msg,
    path_complete::{self, Candidate},
    state::{AppState, DialogKind, Focus},
    tab::{JumpOutcome, Tab, Viewport},
    toast::{Toast, ToastLevel},
};

impl App {
    /// Bookkeeping after any action or message: focus that lost its panel
    /// falls back to the table, the Detected format dialog opens for a tab
    /// that needs it, and the `freeze N (M shown)` notice.
    pub(super) fn after_change(&mut self) {
        self.fix_focus();
        self.maybe_open_detected_format();
        self.check_freeze_notice();
        // `--filter` once the dialect is confirmed (M4-04).
        self.maybe_apply_startup_filter();
        // `--sort` after it (§3).
        self.maybe_apply_startup_sort();
    }

    // ---- pending jumps (M2-03) -----------------------------------------

    /// Performs the pending jumps the index now covers (each tick and on
    /// `IndexReady`).
    pub(super) fn poll_jumps(&mut self) {
        let viewport = self.viewport();
        for i in 0..self.state.tabs.len() {
            if let Some(JumpOutcome::Clamped { len }) = self.state.tabs[i].poll_jump(viewport) {
                self.clamped_toast(len);
            }
        }
    }

    fn clamped_toast(&mut self, len: u64) {
        let text = format!("only {} rows — went to the last row", format_count(len));
        self.state
            .toasts
            .push(Toast::new(ToastLevel::Info, text), now());
    }

    // ---- dialogs -------------------------------------------------------

    /// An action while a dialog is open: it goes to that dialog.
    pub(super) fn dialog_action(&mut self, action: &Action, viewport: Viewport) {
        match self.state.dialog() {
            Some(DialogKind::Goto) => match action {
                Action::Submit => self.goto_submit(viewport),
                Action::Cancel => self.state.close_dialog(),
                _ => {}
            },
            Some(DialogKind::DetectedFormat) => self.detected_action(action),
            Some(DialogKind::ColumnChooser) => self.chooser_action(action, viewport),
            Some(DialogKind::ValuePopup) => self.popup_action(action),
            Some(DialogKind::Export) => self.export_action(action),
            // Confirm is handled by `confirm_action` before this.
            Some(DialogKind::Confirm) if *action == Action::Cancel => {
                self.state.close_dialog();
            }
            Some(DialogKind::Confirm) | None => {}
        }
    }

    /// `g`: the go-to dialog, with an empty input (M2-03).
    pub(super) fn open_goto(&mut self) {
        if self.state.overlay.is_some() || self.state.focus != Focus::Table {
            return;
        }
        if self.state.active_tab().is_some_and(|t| t.loaded.is_some()) {
            self.state.goto = GotoDialog::default();
            self.state.open_dialog(DialogKind::Goto);
        }
    }

    /// `Enter` in the go-to dialog. A parse error, or a hidden column, keeps
    /// the dialog open with the error; anything else runs and closes it.
    fn goto_submit(&mut self, viewport: Viewport) {
        let input = self.state.goto.input.text().to_owned();
        let Some(tab) = self.state.tabs.get_mut(self.state.active_tab) else {
            self.state.close_dialog();
            return;
        };
        let Some(l) = &tab.loaded else {
            self.state.close_dialog();
            return;
        };
        let target = match parse_goto(&input, &l.columns) {
            Ok(t) => t,
            Err(e) => {
                self.state.goto.error = Some(e);
                return;
            }
        };
        tab.pending_jump = None;
        let outcome = match target {
            GotoTarget::Row(n) => tab.goto_row(n, viewport),
            GotoTarget::Percent(p) => tab.goto_percent(p, viewport),
            GotoTarget::Column(c) => match tab.goto_column(c, viewport) {
                Ok(()) => JumpOutcome::Jumped,
                Err(e) => {
                    self.state.goto.error = Some(e);
                    return;
                }
            },
        };
        self.state.close_dialog();
        if let JumpOutcome::Clamped { len } = outcome {
            self.clamped_toast(len);
        }
    }

    /// Whether the Detected format dialog may open at all: not when the
    /// dialect came from the command line, with `--yes`, or with
    /// `sniff.confirm: false` (§2.1, §12.3, §15).
    fn sniff_confirm(&self) -> bool {
        let s = &self.state.settings;
        !s.yes && s.sniff_confirm && s.dialect_overrides.is_empty()
    }

    /// Opens the Detected format dialog for the active tab, the first time
    /// it is active and loaded (M2-04). Never stacks on another overlay or
    /// the `open ›` prompt; it waits for them to close.
    fn maybe_open_detected_format(&mut self) {
        if !self.sniff_confirm() || self.state.overlay.is_some() || self.state.prompt.is_some() {
            return;
        }
        let overrides = self.state.settings.dialect_overrides;
        let Some(tab) = self.state.tabs.get_mut(self.state.active_tab) else {
            return;
        };
        if tab.dialog_shown {
            return;
        }
        let Some(l) = &tab.loaded else {
            return;
        };
        tab.dialog_shown = true;
        if l.source.is_empty() {
            return;
        }
        let current = *l.source.dialect();
        // D3: `Esc` reverts to pure detection with the CLI overrides. A
        // transcoded file stays UTF-8 (its source is the UTF-8 copy).
        let mut detected = overrides.apply(l.sniff.detected);
        if l.original_encoding.is_some() {
            detected = detected.transcoded();
        }
        detected.comment = current.comment;
        self.state.detected = Some(DetectedFormatState::new(tab.id, current, detected));
        self.state.open_dialog(DialogKind::DetectedFormat);
    }

    fn detected_action(&mut self, action: &Action) {
        let t = now();
        let Some(d) = self.state.detected.as_mut() else {
            self.state.close_dialog();
            return;
        };
        let id = d.tab;
        match action {
            Action::CycleDelimiter => d.cycle_delimiter(t),
            Action::ToggleHeader => d.toggle_header(t),
            Action::CycleQuote => d.cycle_quote(t),
            Action::ToggleRaw => {
                if let Some(tab) = self.state.tabs.iter_mut().find(|tab| tab.id == id) {
                    tab.raw_mode = !tab.raw_mode;
                }
            }
            Action::Submit => {
                // Accept: apply an edit still waiting for its debounce.
                let pending = d.apply_at.take().map(|_| d.current);
                self.close_detected();
                if let Some(dialect) = pending {
                    self.apply_if_changed(id, dialect);
                }
            }
            Action::Cancel => {
                // D3: revert to the detected dialect, re-indexing only if
                // the applied one differs.
                let detected = d.detected;
                self.close_detected();
                self.apply_if_changed(id, detected);
            }
            _ => {}
        }
    }

    fn close_detected(&mut self) {
        if let Some(d) = self.state.detected.take()
            && let Some(tab) = self.state.tabs.iter_mut().find(|t| t.id == d.tab)
        {
            tab.raw_mode = false;
        }
        self.state.close_dialog();
    }

    /// Re-indexes tab `id` with `dialect` unless it already uses it.
    fn apply_if_changed(&mut self, id: crate::tab::TabId, dialect: tachy_core::dialect::Dialect) {
        let applied = self
            .state
            .tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.loaded.as_ref())
            .map(|l| *l.source.dialect());
        if applied.is_some_and(|a| a != dialect) {
            self.restart_indexing(id, dialect);
        }
    }

    /// The Detected format edit whose 150 ms debounce has passed (each tick).
    pub(super) fn apply_due_dialect(&mut self, t: std::time::Instant) {
        let Some(d) = self.state.detected.as_mut() else {
            return;
        };
        if let Some(dialect) = d.due(t) {
            let id = d.tab;
            self.apply_if_changed(id, dialect);
        }
    }

    /// `c`: the column chooser over a copy of the active tab's layout.
    pub(super) fn open_chooser(&mut self) {
        if self.state.overlay.is_some() || self.state.focus != Focus::Table {
            return;
        }
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        if tab.loaded.is_none() {
            return;
        }
        self.state.chooser = Some(ColumnChooserState::new(
            tab.id,
            &tab.layout,
            tab.cursor_source_col(),
        ));
        self.state.open_dialog(DialogKind::ColumnChooser);
    }

    fn chooser_action(&mut self, action: &Action, viewport: Viewport) {
        let AppState { tabs, chooser, .. } = &mut self.state;
        let Some(c) = chooser.as_mut() else {
            self.state.close_dialog();
            return;
        };
        let Some(tab) = tabs.iter_mut().find(|t| t.id == c.tab) else {
            self.state.chooser = None;
            self.state.close_dialog();
            return;
        };
        let Some(l) = &tab.loaded else {
            return;
        };
        let columns = &l.columns;
        match action {
            Action::SelectNext => c.select(1, columns),
            Action::SelectPrev => c.select(-1, columns),
            Action::ToggleVisible => c.toggle_visible(columns),
            Action::MoveColumnDown => c.move_selected(true, columns),
            Action::MoveColumnUp => c.move_selected(false, columns),
            Action::ShowAll => c.show_all(),
            Action::FilterList => {
                c.filtering = true;
                c.error = None;
            }
            Action::Submit => {
                let (order, visible) = (c.order.clone(), c.visible.clone());
                tab.set_column_order(order, visible, viewport);
                self.state.chooser = None;
                self.state.close_dialog();
            }
            Action::Cancel => {
                self.state.chooser = None;
                self.state.close_dialog();
            }
            _ => {}
        }
    }

    /// While the chooser's filter is being typed: `Enter` keeps the filter,
    /// `Esc` clears it, arrows still move the selection, other keys edit.
    /// Returns whether the key was used.
    pub(super) fn chooser_filter_key(&mut self, key: KeyEvent) -> bool {
        let AppState { tabs, chooser, .. } = &mut self.state;
        let Some(c) = chooser.as_mut() else {
            return false;
        };
        if !c.filtering {
            return false;
        }
        let ctrl_alt = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Up | KeyCode::Down => return false,
            KeyCode::Enter => c.filtering = false,
            KeyCode::Esc => {
                c.filtering = false;
                c.filter.set("");
            }
            KeyCode::Char(_) if ctrl_alt => return false,
            _ => {
                c.filter.handle_key(key);
            }
        }
        if let Some(l) = tabs
            .iter()
            .find(|t| t.id == c.tab)
            .and_then(|t| t.loaded.as_ref())
        {
            c.clamp(&l.columns);
        }
        self.dirty = true;
        true
    }

    fn popup_action(&mut self, action: &Action) {
        let area = self.area;
        let Some(p) = self.state.value_popup.as_mut() else {
            self.state.close_dialog();
            return;
        };
        let page = ValuePopupState::page(area) as isize;
        match action {
            Action::SelectNext => p.scroll_by(1, area),
            Action::SelectPrev => p.scroll_by(-1, area),
            Action::PageDown => p.scroll_by(page, area),
            Action::PageUp => p.scroll_by(-page, area),
            Action::FirstRow => p.scroll_to(false, area),
            Action::LastRow => p.scroll_to(true, area),
            Action::Cancel => {
                self.state.value_popup = None;
                self.state.close_dialog();
            }
            _ => {}
        }
    }

    // ---- inspector and focus (M3-03) -----------------------------------

    /// The inspector is on screen: toggled on and at least 120 columns.
    pub(super) fn inspector_shown(&self) -> bool {
        self.state.inspector_visible && self.area.width >= INSPECTOR_MIN_TERMINAL_WIDTH
    }

    /// `Tab`: table → inspector (if shown) → jobs drawer (if open) → table.
    pub(super) fn focus_next(&mut self) {
        let inspector = self.inspector_shown();
        let drawer = self.state.jobs_drawer_open;
        self.state.focus = match self.state.focus {
            Focus::Table if inspector => Focus::Inspector,
            Focus::Table | Focus::Inspector if drawer => Focus::JobsDrawer,
            _ => Focus::Table,
        };
    }

    /// A focused panel that is no longer on screen gives focus back to the
    /// table.
    fn fix_focus(&mut self) {
        let lost = match self.state.focus {
            Focus::Table => false,
            Focus::Inspector => !self.inspector_shown(),
            Focus::JobsDrawer => !self.state.jobs_drawer_open,
        };
        if lost {
            self.state.focus = Focus::Table;
        }
    }

    /// `j`/`k` select a RECORD field, `Enter` opens it in the full-value
    /// popup, `Esc` gives focus back to the table.
    pub(super) fn inspector_action(&mut self, action: &Action) {
        let n = self
            .state
            .active_tab()
            .and_then(|t| t.loaded.as_ref())
            .map_or(0, |l| l.columns.len());
        let sel = &mut self.state.inspector.selected;
        match action {
            Action::SelectNext => *sel = (*sel + 1).min(n.saturating_sub(1)),
            Action::SelectPrev => *sel = sel.saturating_sub(1).min(n.saturating_sub(1)),
            Action::Cancel => self.state.focus = Focus::Table,
            Action::OpenValue => self.open_value_popup(),
            _ => {}
        }
    }

    fn open_value_popup(&mut self) {
        let Some(tab) = self.state.active_tab() else {
            return;
        };
        let (Some(l), Some(row)) = (&tab.loaded, tab.cursor_row_data()) else {
            return;
        };
        let Some(meta) = l.columns.get(self.state.inspector.selected) else {
            return;
        };
        let value = row.display(&l.source, meta.source_index);
        let popup = ValuePopupState::new(meta.name.display.clone(), value, self.area);
        self.state.value_popup = Some(popup);
        self.state.open_dialog(DialogKind::ValuePopup);
    }

    // ---- columns (M3-04) -----------------------------------------------

    /// The `freeze 3 (2 shown)` info toast, once per change.
    fn check_freeze_notice(&mut self) {
        let layout = compute_layout(self.area, &self.state.layout_input());
        if layout.too_small {
            // Not drawn yet (or too small to draw): nothing is reduced.
            return;
        }
        let viewport = self.viewport();
        if let Some(tab) = self.state.tabs.get_mut(self.state.active_tab)
            && tab.loaded.is_some()
            && let Some((requested, shown)) = tab.take_freeze_notice(viewport)
        {
            let text = format!("freeze {requested} ({shown} shown)");
            self.state
                .toasts
                .push(Toast::new(ToastLevel::Info, text), now());
        }
    }

    /// `:freeze N` on the active tab (M3-04). The palette command is M6-02.
    pub fn set_freeze(&mut self, n: usize) {
        let viewport = self.viewport();
        if let Some(tab) = self.state.active_tab_mut() {
            tab.set_freeze(n, viewport);
        }
        self.check_freeze_notice();
        self.dirty = true;
    }

    /// `set type <col> <type>` on the active tab (M3-01): `col` is the
    /// source column, `None` clears the override. Returns `false` when there
    /// is no such column. The palette command is M6-02.
    pub fn set_column_type(&mut self, col: usize, ty: Option<ColType>) -> bool {
        let done = self
            .state
            .active_tab_mut()
            .is_some_and(|tab: &mut Tab| tab.set_column_type(col, ty));
        if done && let Some(id) = self.state.active_tab().map(|t| t.id) {
            // A profile running for the old type would be stale (M5-04).
            let profiles: Vec<_> = self
                .state
                .jobs
                .iter()
                .filter(|j| j.tab == id && j.kind == tachy_core::jobs::JobKind::Profile)
                .map(|j| j.id)
                .collect();
            for job in profiles {
                self.state.jobs.cancel(job);
            }
        }
        self.dirty = true;
        done
    }
}

// ---- column edits ----------------------------------------------------------

impl App {
    /// `edit <col>: …` on the active tab (`Tab::append_edit` /
    /// `Tab::undo_edit`).
    pub(super) fn edit_column(&mut self, col: usize, change: EditChange) -> Result<(), String> {
        let tab = self.state.active_tab_mut().ok_or("no file open")?;
        match change {
            EditChange::Append(ops) => tab.append_edit(col, &ops)?,
            EditChange::Undo => {
                tab.undo_edit(col, false)?;
            }
            EditChange::Reset => {
                tab.undo_edit(col, true)?;
            }
        }
        self.edits_changed();
        Ok(())
    }

    /// `reset edits` on the active tab.
    pub(super) fn reset_edits(&mut self) {
        if self
            .state
            .active_tab_mut()
            .is_some_and(|tab| tab.reset_edits())
        {
            self.edits_changed();
        }
    }

    /// After the active tab's edits changed: its Profile jobs were computing
    /// stats of the old values, and a search in flight matched them.
    fn edits_changed(&mut self) {
        let Some(id) = self.state.active_tab().map(|t| t.id) else {
            return;
        };
        let profiles: Vec<_> = self
            .state
            .jobs
            .iter()
            .filter(|j| j.tab == id && j.kind == tachy_core::jobs::JobKind::Profile)
            .map(|j| j.id)
            .collect();
        for job in profiles {
            self.state.jobs.cancel(job);
        }
        self.forget_search(id);
        self.dirty = true;
    }
}

// ---- Export dialog (M6-01) -------------------------------------------------

impl App {
    /// `e`: the Export dialog for the active tab's current view, with its
    /// defaults.
    pub(super) fn open_export(&mut self) {
        if self.state.overlay.is_some() || self.state.prompt.is_some() {
            return;
        }
        let Some(info) = self.state.active_tab().and_then(ExportInfo::from_tab) else {
            return;
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        self.state.export = Some(ExportForm::new(info, &cwd));
        self.state.open_dialog(DialogKind::Export);
    }

    fn close_export(&mut self) {
        self.state.export = None;
        self.state.close_dialog();
    }

    /// The dialog's bound keys: `Enter`, `Esc`, `Tab`, `↓`/`↑`
    /// (`shift-Tab`).
    fn export_action(&mut self, action: &Action) {
        let Some(form) = self.state.export.as_mut() else {
            self.state.close_dialog();
            return;
        };
        match action {
            Action::Submit => {
                let cwd = std::env::current_dir().unwrap_or_default();
                let home = path_complete::home_dir();
                if let Some(req) = form.submit(&cwd, home.as_deref()) {
                    self.close_export();
                    self.submit_export(req);
                }
            }
            Action::Cancel => {
                if form.cancel() {
                    self.close_export();
                }
            }
            Action::Complete => {
                if let CompleteStep::List(input) = form.complete() {
                    self.spawn_export_listing(input);
                }
            }
            Action::SelectNext => form.next_field(),
            Action::SelectPrev => form.prev_field(),
            _ => {}
        }
    }

    /// An unbound key in the dialog: edits the path, changes a choice, or
    /// answers the overwrite confirm.
    pub(super) fn export_key(&mut self, key: KeyEvent) {
        let Some(form) = self.state.export.as_mut() else {
            return;
        };
        match form.handle_key(key) {
            KeyOutcome::Ignored => {}
            KeyOutcome::Changed => self.dirty = true,
            KeyOutcome::Submit(req) => {
                self.close_export();
                self.submit_export(req);
                self.dirty = true;
            }
        }
    }

    /// Lists the directory for the path field off the UI task, with the
    /// prompt's timeout; the result comes back as `Msg::PathCompletions`.
    fn spawn_export_listing(&self, input: String) {
        let cwd = std::env::current_dir().unwrap_or_default();
        let home = path_complete::home_dir();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let listing = tokio::task::spawn_blocking({
                let input = input.clone();
                move || path_complete::list_candidates(&input, &cwd, home.as_deref())
            });
            let result = match tokio::time::timeout(super::COMPLETION_TIMEOUT, listing).await {
                Ok(Ok(Ok(candidates))) => Ok(candidates),
                Ok(Ok(Err(e))) => Err(e.to_string()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("directory listing timed out".to_owned()),
            };
            let _ = tx.send(Msg::PathCompletions { input, result });
        });
    }

    /// A `Msg::PathCompletions` the Export dialog waits for is used here
    /// (`None`); anything else is handed back for the `open ›` prompt.
    pub(super) fn export_completions(
        &mut self,
        input: &str,
        result: Result<Vec<Candidate>, String>,
    ) -> Option<Result<Vec<Candidate>, String>> {
        match self.state.export.as_mut() {
            Some(form) if form.completing.as_deref() == Some(input) => {
                form.completions(input, result);
                None
            }
            _ => Some(result),
        }
    }
}
