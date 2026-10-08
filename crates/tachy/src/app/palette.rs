//! `App` side of the command palette (M6-02): opening it, `Enter`, and
//! running an [`Invocation`]. A child module of `app`, so it shares `App`'s
//! private fields and methods.

use tachy_core::size::format_count;

use super::{App, now};
use crate::{
    commands::Invocation,
    components::palette,
    goto::GotoTarget,
    path_complete,
    tab::JumpOutcome,
    toast::{Toast, ToastLevel},
};

impl App {
    /// `:`: opens the palette with an empty input, unless another overlay
    /// or the `open ›` prompt is up.
    pub(super) fn open_palette(&mut self) {
        if self.state.overlay.is_some() || self.state.prompt.is_some() {
            return;
        }
        self.ui.palette.extra_entries = self.saved_view_entries();
        self.ui.palette.reset();
        palette::open(&mut self.state);
    }

    pub(super) fn close_palette(&mut self) {
        palette::close(&mut self.state);
    }

    /// `Enter` in the palette: runs the command. Invalid arguments keep the
    /// palette open with the error in its preview line.
    pub(super) fn palette_submit(&mut self) {
        let Some(invocation) = self.ui.palette.submit(&self.state) else {
            return;
        };
        // Close first: the command may open a dialog or another overlay.
        palette::close(&mut self.state);
        if let Err(e) = self.run_invocation(invocation) {
            palette::open(&mut self.state);
            self.ui.palette.set_error(e);
        }
    }

    fn run_invocation(&mut self, invocation: Invocation) -> Result<(), String> {
        match invocation {
            Invocation::Action(action) => {
                let _ = self.action_tx.send(action);
            }
            Invocation::Sort(spec) => self.start_sort(spec),
            Invocation::Filter(text) => self.submit_filter_text(text),
            Invocation::SetType { col, ty } => {
                if !self.set_column_type(col, ty) {
                    return Err("no such column".to_owned());
                }
            }
            Invocation::SetHints(on) => self.state.hints_visible = on,
            Invocation::SetInspector(on) => self.state.inspector_visible = on,
            Invocation::Freeze(n) => self.set_freeze(n),
            Invocation::Profile(target) => self.start_profile(target),
            Invocation::Open(path) => {
                let cwd = std::env::current_dir().unwrap_or_default();
                let home = path_complete::home_dir();
                let path = path_complete::resolve(&path, &cwd, home.as_deref());
                self.open_path(path, None);
                self.state.active_tab = self.state.tabs.len() - 1;
            }
            Invocation::Export => self.open_export(),
            Invocation::Goto(target) => self.palette_goto(target)?,
            Invocation::DeleteView(name) => self.delete_view(name),
            Invocation::Edit { col, change } => self.edit_column(col, change)?,
            Invocation::ResetEdits => self.reset_edits(),
            Invocation::Dupes(spec) => self.start_dupes(spec),
        }
        self.dirty = true;
        Ok(())
    }

    /// `goto <row|N%|col>`: the go-to dialog's jump, without the dialog.
    fn palette_goto(&mut self, target: GotoTarget) -> Result<(), String> {
        let viewport = self.viewport();
        let Some(tab) = self.state.active_tab_mut() else {
            return Err("no file open".to_owned());
        };
        tab.pending_jump = None;
        let outcome = match target {
            GotoTarget::Row(n) => tab.goto_row(n, viewport),
            GotoTarget::Percent(p) => tab.goto_percent(p, viewport),
            GotoTarget::Column(c) => tab.goto_column(c, viewport).map(|()| JumpOutcome::Jumped)?,
        };
        if let JumpOutcome::Clamped { len } = outcome {
            let text = format!("only {} rows — went to the last row", format_count(len));
            self.state
                .toasts
                .push(Toast::new(ToastLevel::Info, text), now());
        }
        Ok(())
    }
}
