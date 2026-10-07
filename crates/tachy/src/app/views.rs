//! `App` handlers for saved views (M6-04, D2): `Ctrl-S` in the filter bar
//! saves the filter as a named view in `views.json`; the palette lists the
//! views whose `file_glob` matches the active file (`view: <name>`) and
//! deletes them (`delete view <name>`). A child module of `app`, so it
//! shares `App`'s private fields.

use crossterm::event::{KeyCode, KeyEvent};

use super::{App, now};
use crate::{
    action::Action,
    commands::{self, Command},
    input::LineEdit,
    msg::Msg,
    search_ui::{BarKind, SaveFlow, SaveStep, compile_filter},
    toast::{Toast, ToastLevel},
    views_store::{self, NameStatus, ViewSource},
};

impl App {
    /// Loads `views.json` and the config's `views` (M6-04). A corrupt file
    /// becomes a warning toast. Tests start with an empty store, so the
    /// developer's own views never leak into them.
    pub(super) fn load_views(&mut self) {
        if cfg!(test) {
            return;
        }
        let dir = crate::config::get_config_dir();
        let (store, warning) = views_store::ViewsStore::load(&dir, &self.config.app.views);
        self.state.views = store;
        if let Some(w) = warning {
            self.state.toasts.push(w.toast(), now());
        }
    }

    /// The tab's file name as `file_glob` sees it (`""` for stdin).
    fn active_match_name(&self) -> Option<String> {
        let tab = self.state.active_tab()?;
        let path = (tab.temp.is_none()).then_some(tab.path.as_path());
        Some(views_store::match_name(path))
    }

    /// The `view: <name>` palette entries for the active tab.
    pub(super) fn saved_view_entries(&self) -> Vec<Command> {
        let Some(name) = self.active_match_name() else {
            return Vec::new();
        };
        self.state
            .views
            .matching_views(&name)
            .into_iter()
            .map(|v| {
                let label = match v.source {
                    ViewSource::Store => v.name.clone(),
                    ViewSource::Config => format!("{} (config)", v.name),
                };
                commands::view_entry(&label, &v.filter)
            })
            .collect()
    }

    /// `delete view <name>` from the palette.
    pub(super) fn delete_view(&mut self, name: String) {
        let store = self.state.views.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let done = format!("deleted view \"{name}\"");
            let (store, result) = views_store::delete_async(store, name).await;
            let _ = tx.send(Msg::ViewsWritten {
                store: Box::new(store),
                result,
                done,
            });
        });
    }

    /// `Ctrl-S` in the filter bar: starts the save flow when the filter
    /// parses and resolves; otherwise shows `fix the filter before saving`.
    pub(super) fn start_save_view(&mut self) {
        let Some(bar) = self.state.find.bar.as_ref() else {
            return;
        };
        if bar.kind != BarKind::Filter || self.state.find.save.is_some() {
            return;
        }
        let expr = bar.edit.text().trim().to_owned();
        let valid = self
            .state
            .tabs
            .iter()
            .find(|t| t.id == bar.tab)
            .is_some_and(|t| !expr.is_empty() && compile_filter(&expr, t).is_ok());
        if !valid {
            self.set_bar_error("fix the filter before saving");
            return;
        }
        let file = self.active_match_name().unwrap_or_default();
        let glob = views_store::default_glob((!file.is_empty()).then_some(file.as_str()));
        self.state.find.save = Some(SaveFlow {
            expr,
            step: SaveStep::Name,
            name: LineEdit::default(),
            glob: LineEdit::with_text(glob),
            error: None,
            note: None,
        });
    }

    fn set_bar_error(&mut self, message: &str) {
        if let Some(bar) = self.state.find.bar.as_mut() {
            bar.validation = crate::search_ui::Validation::Invalid {
                message: message.to_owned(),
                span: None,
            };
            bar.validate_at = None;
        }
    }

    /// Actions while the save flow is open: `Enter` goes to the next step,
    /// `Esc` goes back to editing the filter. Returns whether it was used.
    pub(super) fn save_view_action(&mut self, action: &Action) -> bool {
        let Some(flow) = self.state.find.save.as_mut() else {
            return false;
        };
        match action {
            Action::Submit => self.save_view_next(),
            Action::Cancel => self.state.find.save = None,
            // History, completion, `Ctrl-S` again: ignored.
            _ => {
                let _ = flow;
            }
        }
        true
    }

    /// A key not bound in the `Filter` context while saving: edits the name
    /// or the glob, or answers `y` / `n`. Returns whether it was used.
    pub(super) fn save_view_key(&mut self, key: KeyEvent) -> bool {
        let Some(flow) = self.state.find.save.as_mut() else {
            return false;
        };
        match flow.step {
            SaveStep::Name => {
                flow.name.handle_key(key);
                flow.error = None;
            }
            SaveStep::Glob => {
                flow.glob.handle_key(key);
                flow.error = None;
            }
            SaveStep::Overwrite => match key.code {
                KeyCode::Char('y' | 'Y') => self.write_view(),
                KeyCode::Char('n' | 'N') => flow.step = SaveStep::Name,
                _ => {}
            },
            SaveStep::Writing => {}
        }
        true
    }

    fn save_view_next(&mut self) {
        let Some(flow) = self.state.find.save.as_mut() else {
            return;
        };
        match flow.step {
            SaveStep::Name => match views_store::validate_name(flow.name.text()) {
                Ok(_) => {
                    flow.step = SaveStep::Glob;
                    flow.error = None;
                }
                Err(e) => flow.error = Some(e.to_string()),
            },
            SaveStep::Glob => {
                if let Err(e) = views_store::validate_glob(flow.glob.text()) {
                    flow.error = Some(e);
                    return;
                }
                let name = flow.name.text().trim().to_owned();
                match self.state.views.name_status(&name) {
                    NameStatus::Stored => {
                        if let Some(flow) = self.state.find.save.as_mut() {
                            flow.step = SaveStep::Overwrite;
                        }
                    }
                    status => {
                        if status == NameStatus::ConfigOnly
                            && let Some(flow) = self.state.find.save.as_mut()
                        {
                            flow.note = Some(format!(
                                "\"{name}\" will shadow the view of the same name in the config"
                            ));
                        }
                        self.write_view();
                    }
                }
            }
            SaveStep::Overwrite => self.write_view(),
            SaveStep::Writing => {}
        }
    }

    /// Writes the view to `views.json` off the UI task.
    fn write_view(&mut self) {
        let Some(flow) = self.state.find.save.as_mut() else {
            return;
        };
        flow.step = SaveStep::Writing;
        let name = flow.name.text().trim().to_owned();
        let glob = flow.glob.text().trim().to_owned();
        let expr = flow.expr.clone();
        let store = self.state.views.clone();
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let done = format!("saved view \"{name}\"");
            let (store, result) = views_store::upsert_async(store, name, glob, expr).await;
            let _ = tx.send(Msg::ViewsWritten {
                store: Box::new(store),
                result,
                done,
            });
        });
    }

    /// `Msg::ViewsWritten`: keeps the updated store. A save closes the flow
    /// (back to editing the filter: saving does not apply it); an error is
    /// shown inline (or as a toast for a delete).
    pub(super) fn views_written(
        &mut self,
        store: views_store::ViewsStore,
        result: Result<views_store::SaveReport, views_store::ViewsError>,
        done: String,
    ) {
        self.state.views = store;
        match result {
            Ok(report) => {
                let saving = self
                    .state
                    .find
                    .save
                    .as_ref()
                    .is_some_and(|f| f.step == SaveStep::Writing);
                if saving {
                    self.state.find.save = None;
                }
                let mut text = done;
                if let Some(backup) = report.backup {
                    text.push_str(&format!(
                        " (the corrupt file was kept as {})",
                        backup.display()
                    ));
                }
                self.state
                    .toasts
                    .push(Toast::new(ToastLevel::Info, text), now());
            }
            Err(e) => match self.state.find.save.as_mut() {
                Some(flow) if flow.step == SaveStep::Writing => {
                    flow.step = SaveStep::Name;
                    flow.error = Some(e.to_string());
                }
                _ => self
                    .state
                    .toasts
                    .push(Toast::new(ToastLevel::Error, e.to_string()), now()),
            },
        }
    }
}
