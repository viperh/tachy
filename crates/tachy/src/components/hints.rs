//! Key-hint line (spec §11.6, M1-07).
//!
//! `key action` pairs for the current key context, separated by two spaces,
//! with `? help` pinned at the right. Keys come from the live keymap, so
//! remapped keys show correctly (M6-03). When the line is too narrow, whole
//! pairs are dropped from the right; `? help` never is.

use ratatui::{Frame, layout::Rect};
use tachy_core::text;

use super::{Component, put};
use crate::{action::Action, config::Config, keymap::Keymap, mode::KeyContext, state::AppState};

const GAP: &str = "  ";

/// Draws the hint line.
#[derive(Debug, Default)]
pub struct Hints {
    keymap: Keymap,
}

/// The pairs for `context`: the actions whose keys are shown together, and
/// the label.
fn pairs(context: KeyContext) -> &'static [(&'static [Action], &'static str)] {
    use Action::*;
    match context {
        KeyContext::Normal => &[
            (&[Search], "search"),
            (&[Filter], "filter"),
            (&[SortAsc], "sort"),
            (&[ToggleInspector], "inspect"),
            (&[CommandPalette], "command"),
            (&[ToggleJobs], "jobs"),
            (&[Quit], "quit"),
        ],
        KeyContext::Filter | KeyContext::Search => &[
            (&[Submit], "apply"),
            (&[Cancel], "cancel"),
            (&[HistoryPrev, HistoryNext], "history"),
            (&[Complete], "complete"),
        ],
        KeyContext::Command => &[
            (&[SelectPrev, SelectNext], "select"),
            (&[Complete], "args"),
            (&[Submit], "run"),
            (&[Cancel], "close"),
        ],
        KeyContext::Prompt => &[
            (&[Complete], "complete"),
            (&[Submit], "open"),
            (&[Cancel], "cancel"),
        ],
        KeyContext::Inspector => &[
            (&[SelectNext, SelectPrev], "select"),
            (&[OpenValue], "full value"),
            (&[Cancel], "back"),
            (&[FocusNext], "focus"),
        ],
        KeyContext::JobsDrawer => &[
            (&[SelectNext, SelectPrev], "select"),
            (&[PauseJob], "pause"),
            (&[KillJob], "kill"),
            (&[DismissJob], "dismiss"),
            (&[Cancel], "back"),
        ],
        KeyContext::Help => &[(&[Cancel], "close")],
        KeyContext::Goto => &[(&[Submit], "go"), (&[Cancel], "cancel")],
        KeyContext::DetectedFormat => &[
            (&[Submit], "accept"),
            (&[Cancel], "revert"),
            (&[CycleDelimiter], "delimiter"),
            (&[ToggleHeader], "header"),
            (&[CycleQuote], "quoting"),
            (&[ToggleRaw], "raw"),
        ],
        KeyContext::ColumnChooser => &[
            (&[ToggleVisible], "show/hide"),
            (&[MoveColumnDown, MoveColumnUp], "move"),
            (&[ShowAll], "all"),
            (&[FilterList], "filter"),
            (&[Submit], "apply"),
            (&[Cancel], "cancel"),
        ],
        KeyContext::ValuePopup => &[
            (&[SelectNext, SelectPrev], "scroll"),
            (&[LastRow], "end"),
            (&[Cancel], "close"),
        ],
        KeyContext::Confirm => &[(&[Submit], "yes"), (&[Cancel], "no")],
        // ←/→ cycle the focused option; Tab completes the path (M6-01).
        KeyContext::Export => &[
            (&[SelectNext, SelectPrev], "field"),
            (&[Complete], "complete"),
            (&[Submit], "export"),
            (&[Cancel], "cancel"),
        ],
    }
}

impl Hints {
    /// The key text of a pair: the shown chord of each action, joined
    /// (`↑↓`). `None` when an action is unbound in `context`.
    fn keys(&self, context: KeyContext, actions: &[Action]) -> Option<String> {
        actions
            .iter()
            .map(|a| self.keymap.display_chord(context, a))
            .collect()
    }

    /// The visible pairs for `context`, in order.
    pub fn line(&self, context: KeyContext) -> Vec<(String, &'static str)> {
        pairs(context)
            .iter()
            .filter_map(|(actions, label)| Some((self.keys(context, actions)?, *label)))
            .collect()
    }

    /// The key that opens help: from `context`, else Normal, else `?`.
    fn help_key(&self, context: KeyContext) -> String {
        self.keymap
            .display_chord(context, &Action::Help)
            .or_else(|| self.keymap.display_chord(KeyContext::Normal, &Action::Help))
            .unwrap_or_else(|| "?".to_owned())
    }
}

impl Component for Hints {
    fn register_config_handler(&mut self, config: Config) -> color_eyre::Result<()> {
        self.keymap = config.keybindings;
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let buf = frame.buffer_mut();
        let base = theme.base();
        buf.set_style(area, base);
        let context = state.key_context();
        let y = area.y;

        let help_key = self.help_key(context);
        let help_w = text::width(&help_key) + 1 + "help".len() + 1;
        let help_x = area.right().saturating_sub(help_w as u16);
        let x = put(buf, help_x, y, &help_key, base.patch(theme.hint_key()));
        put(buf, x, y, " help", base.patch(theme.hint()));

        // Pairs from the left, whole pairs only, leaving a gap before help.
        let limit = help_x.saturating_sub(GAP.len() as u16);
        let mut x = area.x + 1;
        for (i, (key, label)) in self.line(context).into_iter().enumerate() {
            let gap = if i == 0 { 0 } else { GAP.len() };
            let w = gap + text::width(&key) + 1 + text::width(label);
            if usize::from(x) + w > usize::from(limit) {
                break;
            }
            x += gap as u16;
            x = put(buf, x, y, &key, base.patch(theme.hint_key()));
            x = put(buf, x, y, &format!(" {label}"), base.patch(theme.hint()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints() -> Hints {
        let mut h = Hints::default();
        h.register_config_handler(Config::embedded()).unwrap();
        h
    }

    fn text(h: &Hints, context: KeyContext) -> String {
        h.line(context)
            .into_iter()
            .map(|(k, l)| format!("{k} {l}"))
            .collect::<Vec<_>>()
            .join(GAP)
    }

    #[test]
    fn default_lines() {
        let h = hints();
        assert_eq!(
            text(&h, KeyContext::Normal),
            "/ search  f filter  s sort  i inspect  : command  J jobs  q quit"
        );
        assert_eq!(
            text(&h, KeyContext::Filter),
            "Enter apply  Esc cancel  ↑↓ history  Tab complete"
        );
        assert_eq!(
            text(&h, KeyContext::Command),
            "↑↓ select  Tab args  Enter run  Esc close"
        );
        assert_eq!(h.help_key(KeyContext::Filter), "?");
    }

    #[test]
    fn remapped_keys_show() {
        let mut config = Config::embedded();
        let normal = config.keybindings.0.get_mut(&KeyContext::Normal).unwrap();
        normal.retain(|_, a| *a != Action::Search);
        normal.insert(
            crate::keymap::parse_key_chord("ctrl-f").unwrap(),
            Action::Search,
        );
        let mut h = Hints::default();
        h.register_config_handler(config).unwrap();
        assert!(text(&h, KeyContext::Normal).starts_with("ctrl-f search  f filter"));
    }
}
