//! Command palette overlay (spec §11.7, §12.5, M6-02).
//!
//! `:` opens it ([`open`]), `Esc` closes it ([`close`]); `App` routes
//! `Enter` to [`Palette::submit`] and runs the returned [`Invocation`].
//! `↑`/`↓` and `Tab` are handled here, in `update`. Unbound keys edit the
//! input.
//!
//! - **Name mode**: the input fuzzy-matches command names.
//! - **Argument mode**: once the input is a full command name plus a space,
//!   the list shows completions of the argument being typed and the bottom
//!   line validates the arguments live (the sort estimate, or the error in
//!   coral).
//!
//! Layout inside the purple border (`layout::palette_rect`): line 1 is
//! `› ` and the input, then the list, then the context preview line.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Clear},
};
use tachy_core::text;

use super::{Component, put};
use crate::{
    action::Action,
    commands::{
        Command, CompletionCache, Invocation, Preview, fuzzy_match, key_for, registry,
        split_command,
    },
    config::Config,
    input::LineInput,
    keymap::Keymap,
    mode::Mode,
    state::{AppState, Overlay},
    tui::Event,
};

const PROMPT: &str = "› ";
/// Widest name column; longer names are truncated.
const NAME_WIDTH_MAX: usize = 28;

/// Opens the palette over whatever is on screen (the jobs drawer stays
/// open, §12.5). `mode = Command` while it is open (§12.1).
pub fn open(state: &mut AppState) {
    if state.overlay.is_some() {
        return;
    }
    state.overlay = Some(Overlay::Palette);
    state.mode = Mode::Command;
}

/// Closes the palette and goes back to Normal mode.
pub fn close(state: &mut AppState) {
    if state.overlay == Some(Overlay::Palette) {
        state.overlay = None;
    }
    if state.mode == Mode::Command {
        state.mode = Mode::Normal;
    }
}

/// What a list row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// Index into `Palette::commands`.
    Command(usize),
    /// An argument completion: the text replacing the current argument
    /// (with its suffix).
    Completion(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    entry: Entry,
    label: String,
    /// Matched character indices of `label`, drawn in `palette_match`.
    matched: Vec<usize>,
    description: String,
    key: Option<String>,
}

/// The palette's content for the current input.
#[derive(Debug, Clone)]
struct Model {
    /// Argument mode: the command and the byte offset of its arguments.
    arg: Option<(usize, usize)>,
    /// Argument mode: offset in the input where the current argument starts.
    arg_start: usize,
    rows: Vec<Row>,
    preview: Option<Preview>,
}

/// The command palette: the registry, the input, the selection.
#[derive(Debug, Default)]
pub struct Palette {
    keymap: Keymap,
    /// Entries added after the built-in ones: one `view: <name>` per saved
    /// view matching the active file (M6-04). Set before opening.
    pub extra_entries: Vec<Command>,
    commands: Vec<Command>,
    pub input: LineInput,
    selected: usize,
    /// First list row shown.
    scroll: usize,
    /// The error of the last `Enter`, until the input changes.
    error: Option<String>,
    cache: CompletionCache,
}

impl Palette {
    /// A palette showing bindings from `keymap` (the merged keymap, M6-03).
    pub fn new(keymap: Keymap) -> Self {
        let mut p = Self {
            keymap,
            ..Self::default()
        };
        p.reset();
        p
    }

    /// Clears the input and rebuilds the command list (with the current
    /// `extra_entries`). Called each time the palette opens.
    pub fn reset(&mut self) {
        self.commands = registry(&self.extra_entries);
        self.input = LineInput::default();
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
        self.cache = CompletionCache::default();
    }

    /// Shows `error` in the preview line (a command that failed to run).
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn model(&mut self, state: &AppState) -> Model {
        let input = self.input.text().to_owned();
        if let Some((i, at)) = split_command(&self.commands, &input) {
            let cmd = &self.commands[i];
            let args = &input[at..];
            let (start, completions) = cmd.completions(state, args, &mut self.cache);
            let typed = args.get(start..).unwrap_or("").chars().count();
            let rows = completions
                .into_iter()
                .map(|c| Row {
                    entry: Entry::Completion(format!("{}{}", c.text, c.suffix)),
                    matched: (0..typed.min(c.text.chars().count())).collect(),
                    label: c.text,
                    description: c.description,
                    key: None,
                })
                .collect();
            return Model {
                arg: Some((i, at)),
                arg_start: at + start,
                rows,
                preview: Some(cmd.preview(state, args)),
            };
        }
        let rows: Vec<Row> = fuzzy_match(&self.commands, &input)
            .into_iter()
            .map(|(i, matched)| {
                let c = &self.commands[i];
                Row {
                    entry: Entry::Command(i),
                    label: c.name.clone(),
                    matched,
                    description: c.description.clone(),
                    key: c.action.as_ref().and_then(|a| key_for(&self.keymap, a)),
                }
            })
            .collect();
        let preview = match rows.get(self.selected).map(|r| &r.entry) {
            Some(Entry::Command(i)) => {
                let c = &self.commands[*i];
                Some(Preview::Info(if c.takes_args() {
                    format!("usage: {}", c.usage())
                } else {
                    c.description.clone()
                }))
            }
            _ if !input.trim().is_empty() => Some(Preview::Error("no matching command".to_owned())),
            _ => None,
        };
        Model {
            arg: None,
            arg_start: 0,
            rows,
            preview,
        }
    }

    fn edited(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
    }

    /// `↑` / `↓`: moves the selection, wrapping around.
    fn select(&mut self, delta: isize, state: &AppState) {
        let n = self.model(state).rows.len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
    }

    /// `Tab`: in name mode, `name ` of the selected command; in argument
    /// mode, the selected completion in place of the current argument.
    fn complete(&mut self, state: &AppState) {
        let model = self.model(state);
        let Some(row) = model.rows.get(self.selected) else {
            return;
        };
        match &row.entry {
            Entry::Command(i) => {
                let text = format!("{} ", self.commands[*i].name);
                self.input.set(text);
            }
            Entry::Completion(text) => {
                let head = self.input.text()[..model.arg_start].to_owned();
                self.input.set(format!("{head}{text}"));
            }
        }
        self.edited();
    }

    /// `Enter`: the command to run, or `None` when the palette stays open
    /// (an error is then shown in the preview line, or a command that needs
    /// arguments was filled in).
    pub fn submit(&mut self, state: &AppState) -> Option<Invocation> {
        let model = self.model(state);
        let input = self.input.text().to_owned();
        let result = match model.arg {
            Some((i, at)) => self.commands[i].parse(state, &input[at..]),
            None => match model.rows.get(self.selected).map(|r| &r.entry) {
                Some(Entry::Command(i)) => {
                    let c = &self.commands[*i];
                    if c.runs_without_args() {
                        c.parse(state, "")
                    } else {
                        // Needs arguments: fill in `name ` instead.
                        self.complete(state);
                        return None;
                    }
                }
                _ if input.trim().is_empty() => return None,
                _ => Err("no matching command".to_owned()),
            },
        };
        match result {
            Ok(invocation) => Some(invocation),
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }
}

impl Component for Palette {
    fn register_config_handler(&mut self, config: Config) -> color_eyre::Result<()> {
        let extra = std::mem::take(&mut self.extra_entries);
        *self = Palette::new(config.keybindings);
        self.extra_entries = extra;
        Ok(())
    }

    fn handle_events(
        &mut self,
        event: Option<Event>,
        state: &AppState,
    ) -> color_eyre::Result<Option<Action>> {
        // Keys reach `handle_key_event` through `App`; a paste lands here.
        if let Some(Event::Paste(text)) = event
            && state.overlay == Some(Overlay::Palette)
        {
            self.input.insert(&text);
            self.edited();
        }
        Ok(None)
    }

    fn handle_key_event(
        &mut self,
        key: crossterm::event::KeyEvent,
        _state: &AppState,
    ) -> color_eyre::Result<Option<Action>> {
        if self.input.handle_key(key) {
            self.edited();
        }
        Ok(None)
    }

    fn update(
        &mut self,
        action: &Action,
        state: &mut AppState,
    ) -> color_eyre::Result<Option<Action>> {
        if state.overlay != Some(Overlay::Palette) {
            return Ok(None);
        }
        match action {
            Action::SelectNext => self.select(1, state),
            Action::SelectPrev => self.select(-1, state),
            Action::Complete => self.complete(state),
            _ => {}
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let base = theme.dialog();
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .style(base)
            .border_style(theme.palette_border());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width < 4 {
            return Ok(());
        }
        let model = self.model(state);
        let buf = frame.buffer_mut();
        let right = inner.right().saturating_sub(1);

        // Line 1: `› `, the input and a block cursor; long input scrolls so
        // the cursor stays visible.
        let x0 = put(
            buf,
            inner.x + 1,
            inner.y,
            PROMPT,
            base.patch(theme.palette_border()),
        );
        let avail = usize::from(right.saturating_sub(x0));
        let (before, after) = self.input.text().split_at(self.input.cursor());
        let mut shown = before.to_owned();
        while text::width(&shown) + 1 > avail && !shown.is_empty() {
            shown.remove(0);
        }
        let x = put(buf, x0, inner.y, &shown, base);
        let mut chars = after.chars();
        let under = chars.next().map_or(" ".to_owned(), |c| c.to_string());
        let x = put(buf, x, inner.y, &under, theme.cursor_cell());
        let rest = text::truncate_end(chars.as_str(), usize::from(right.saturating_sub(x)));
        put(buf, x, inner.y, &rest, base);

        // Bottom line: the context preview (or the last error).
        let has_preview = inner.height >= 3;
        let preview_y = inner.bottom() - 1;
        if has_preview {
            let width = usize::from(right.saturating_sub(inner.x + 1));
            let (line, style) = match (&self.error, &model.preview) {
                (Some(e), _) | (None, Some(Preview::Error(e))) => {
                    (e.as_str(), theme.inline_error())
                }
                (None, Some(Preview::Info(s))) => (s.as_str(), theme.dim()),
                (None, None) => ("", base),
            };
            let line = text::truncate_end(line, width);
            put(buf, inner.x + 1, preview_y, &line, base.patch(style));
        }

        // The list, between the input and the preview line.
        let top = inner.y + 1;
        let bottom = if has_preview {
            preview_y
        } else {
            inner.bottom()
        };
        let height = usize::from(bottom.saturating_sub(top));
        if height == 0 {
            return Ok(());
        }
        if model.rows.is_empty() {
            if model.arg.is_none() {
                put(
                    buf,
                    inner.x + 1,
                    top,
                    "no matching commands",
                    base.patch(theme.dim()),
                );
            }
            return Ok(());
        }
        self.selected = self.selected.min(model.rows.len() - 1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
        self.scroll = self.scroll.min(model.rows.len().saturating_sub(height));
        let name_width = model
            .rows
            .iter()
            .map(|r| text::width(&r.label))
            .max()
            .unwrap_or(0)
            .min(NAME_WIDTH_MAX);
        for (n, row) in model.rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = top + (n - self.scroll) as u16;
            let row_style = if n == self.selected {
                base.patch(theme.palette_item_selected())
            } else {
                base
            };
            buf.set_style(Rect::new(inner.x, y, inner.width, 1), row_style);
            let key_width = row.key.as_deref().map_or(0, text::width) as u16;
            let key_x = right.saturating_sub(key_width);
            if let Some(key) = &row.key {
                put(buf, key_x, y, key, row_style.patch(theme.hint_key()));
            }
            let x = draw_label(
                buf,
                inner.x + 1,
                y,
                row,
                name_width,
                (row_style, row_style.patch(theme.palette_match())),
            );
            let room = usize::from(key_x.saturating_sub(x + 2));
            let desc = text::truncate_end(&row.description, room);
            put(buf, x + 2, y, &desc, row_style.patch(theme.dim()));
        }
        Ok(())
    }
}

/// Draws `row.label`, truncated to `width`, with its matched characters in
/// the second style. Returns the column after `width` cells.
fn draw_label(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    row: &Row,
    width: usize,
    (style, matched): (Style, Style),
) -> u16 {
    let label = text::truncate_end(&row.label, width);
    let mut cx = x;
    for (i, c) in label.chars().enumerate() {
        let s = if row.matched.contains(&i) {
            matched
        } else {
            style
        };
        cx = put(buf, cx, y, c.encode_utf8(&mut [0; 4]), s);
    }
    x + width as u16
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pretty_assertions::assert_eq;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        commands::tests::{empty_state, state_with_tab},
        components::layout::palette_rect,
    };

    fn palette() -> Palette {
        Palette::new(Config::embedded().keybindings)
    }

    fn type_text(p: &mut Palette, state: &AppState, s: &str) {
        for c in s.chars() {
            p.handle_key_event(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), state)
                .unwrap();
        }
    }

    fn act(p: &mut Palette, state: &mut AppState, action: Action) {
        p.update(&action, state).unwrap();
    }

    fn render(p: &mut Palette, state: &AppState) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                let area = palette_rect(f.area());
                p.draw(f, area, state).unwrap();
            })
            .unwrap();
        terminal
    }

    fn selected_label(p: &mut Palette, state: &AppState) -> String {
        let model = p.model(state);
        model.rows[p.selected].label.clone()
    }

    #[test]
    fn open_and_close_set_the_mode() {
        let mut state = empty_state();
        open(&mut state);
        assert_eq!(state.overlay, Some(Overlay::Palette));
        assert_eq!(state.mode, Mode::Command);
        assert_eq!(state.visible_mode().label(), "COMMAND");
        close(&mut state);
        assert_eq!(state.overlay, None);
        assert_eq!(state.mode, Mode::Normal);
    }

    #[test]
    fn srt_tab_sort_price_desc_enter() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        type_text(&mut p, &state, "srt");
        assert_eq!(selected_label(&mut p, &state), "sort");
        act(&mut p, &mut state, Action::Complete);
        assert_eq!(p.input.text(), "sort ");
        type_text(&mut p, &state, "price:desc");
        let preview = p.model(&state).preview;
        assert!(
            matches!(&preview, Some(Preview::Info(t)) if t.starts_with("est. ")),
            "{preview:?}"
        );
        let Some(Invocation::Sort(spec)) = p.submit(&state) else {
            panic!("no sort");
        };
        assert_eq!((spec.keys[0].column, spec.keys[0].descending), (1, true));
    }

    #[test]
    fn enter_on_a_command_that_needs_arguments_fills_it_in() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        type_text(&mut p, &state, "freez");
        assert_eq!(p.submit(&state), None);
        assert_eq!(p.input.text(), "freeze ");
        type_text(&mut p, &state, "x");
        assert_eq!(p.submit(&state), None);
        assert!(p.error.as_deref().unwrap().contains("number"));
        // Editing clears the error.
        p.handle_key_event(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &state,
        )
        .unwrap();
        assert_eq!(p.error, None);
        type_text(&mut p, &state, "2");
        assert_eq!(p.submit(&state), Some(Invocation::Freeze(2)));
    }

    #[test]
    fn selection_wraps_and_tab_completes_arguments() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        type_text(&mut p, &state, "set type pr");
        act(&mut p, &mut state, Action::Complete);
        assert_eq!(p.input.text(), "set type price ");
        type_text(&mut p, &state, "s");
        act(&mut p, &mut state, Action::Complete);
        assert_eq!(p.input.text(), "set type price str");
        // Name mode: ↑ from the first entry wraps to the last.
        p.reset();
        let n = p.model(&state).rows.len();
        act(&mut p, &mut state, Action::SelectPrev);
        assert_eq!(p.selected, n - 1);
        act(&mut p, &mut state, Action::SelectNext);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn enter_runs_a_plain_action() {
        let state = empty_state();
        let mut p = palette();
        type_text(&mut p, &state, "insp");
        let label = selected_label(&mut p, &state);
        assert!(label.contains("inspector"), "{label}");
        p.reset();
        type_text(&mut p, &state, "quit");
        assert_eq!(p.submit(&state), Some(Invocation::Action(Action::Quit)));
        p.reset();
        type_text(&mut p, &state, "zzzz");
        assert_eq!(p.submit(&state), None);
        assert_eq!(p.error.as_deref(), Some("no matching command"));
    }

    #[test]
    fn snapshot_empty_input() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        insta::assert_snapshot!(render(&mut p, &state).backend());
    }

    #[test]
    fn snapshot_fuzzy_results() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        type_text(&mut p, &state, "srt");
        insta::assert_snapshot!(render(&mut p, &state).backend());
    }

    #[test]
    fn snapshot_argument_mode_with_preview() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        type_text(&mut p, &state, "sort price:desc, ");
        insta::assert_snapshot!(render(&mut p, &state).backend());
    }

    #[test]
    fn snapshot_error() {
        let (_f, mut state) = state_with_tab();
        open(&mut state);
        let mut p = palette();
        type_text(&mut p, &state, "sort nope");
        let t = render(&mut p, &state);
        // The error is drawn in coral (`inline_error`).
        let buf = t.backend().buffer();
        let area = palette_rect(buf.area);
        let y = area.bottom() - 2;
        let x = area.x + 2;
        assert_eq!(buf[(x, y)].fg, state.theme.inline_error().fg.unwrap());
        insta::assert_snapshot!(t.backend());
    }
}
