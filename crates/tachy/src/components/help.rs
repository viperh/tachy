//! Full-screen help overlay (spec §11.7, M7-02): every key binding, grouped
//! by context, generated from the live keymap (`help_model`).
//!
//! `?` (`Help`) opens it from Normal mode; `?` or `Esc` closes it. While it
//! is open the mode is `Dialog`, so the pill keeps the previous mode (§12.1).
//! 3 columns at width ≥ 160, 2 at ≥ 100, else 1; groups flow top to bottom,
//! then across columns, and the whole content scrolls when taller than the
//! screen. A query cheat sheet (§9) and the `g 50%` note (M2-03) stay at the
//! bottom.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Clear},
};
use tachy_core::text;

use super::{Component, put};
use crate::{
    action::Action,
    config::Config,
    help_model::{self, HelpGroup},
    keymap::Keymap,
    mode::Mode,
    state::{AppState, Overlay},
    theme::Theme,
};

/// Gap between columns.
const COLUMN_GAP: u16 = 3;
/// The key column never takes more than 1/`MAX_KEY_SHARE` of a column.
const MAX_KEY_SHARE: u16 = 2;

/// A footer item: pieces of `(text, is_key)`, joined with one space.
type Item = &'static [(&'static str, bool)];

/// §9 cheat sheet. Items wrap as a whole, separated by two spaces.
const CHEAT_SHEET: &[Item] = &[
    &[("query:", false)],
    &[("== != < <= > >=", true)],
    &[("~", true), ("regex", false)],
    &[("contains starts ends", true)],
    &[("\"x\"i", true), ("case-insensitive", false)],
    &[("in [..]", true)],
    &[("is [not] null", true)],
    &[("&& || !", true)],
    &[("and or not", true)],
    &[("`col name`", true)],
    &[("$1", true)],
];

/// The go-to note (M2-03): `50%` is a byte position while indexing.
const GOTO_NOTE: &[Item] = &[
    &[("goto:", false)],
    &[("g 50%", true)],
    &[("= the middle of the file by bytes while indexing,", false)],
    &[("the middle row once indexed", false)],
];

/// Opens the help over whatever is on screen. The pill keeps the current
/// mode (§12.1). No-op if another overlay is open.
pub fn open(state: &mut AppState) {
    if state.overlay.is_some() {
        return;
    }
    if state.mode != Mode::Dialog {
        state.pill_mode = state.mode;
    }
    state.mode = Mode::Dialog;
    state.overlay = Some(Overlay::Help);
}

/// Closes the help and restores the mode it was opened from.
pub fn close(state: &mut AppState) {
    if state.overlay == Some(Overlay::Help) {
        state.overlay = None;
        if state.mode == Mode::Dialog {
            state.mode = state.pill_mode;
        }
    }
}

/// The help screen. Keeps the model built from the keymap and the scroll
/// position.
#[derive(Debug, Default)]
pub struct Help {
    keymap: Keymap,
    groups: Vec<HelpGroup>,
    /// First content line shown.
    scroll: usize,
    /// From the last draw: the largest useful `scroll`, and the page height.
    max_scroll: usize,
    page: usize,
}

impl Help {
    /// A help screen for `keymap` (the merged keymap, M6-03).
    pub fn new(keymap: Keymap) -> Self {
        let groups = help_model::build(&keymap);
        Self {
            keymap,
            groups,
            ..Self::default()
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll);
    }
}

/// Columns for a screen `width` cells wide.
pub fn column_count(width: u16) -> u16 {
    match width {
        160.. => 3,
        100.. => 2,
        _ => 1,
    }
}

/// One rendered line of a group.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Title(&'static str),
    Entry { keys: String, desc: String },
    Note(String),
    Blank,
}

/// The lines of `group` in a column `width` cells wide (notes wrapped),
/// with a trailing blank line.
fn group_lines(group: &HelpGroup, width: usize) -> Vec<Line> {
    let mut lines = vec![Line::Title(group.title)];
    lines.extend(group.entries.iter().map(|(k, d)| Line::Entry {
        keys: k.clone(),
        desc: d.clone(),
    }));
    for note in &group.notes {
        lines.extend(wrap_words(note, width).into_iter().map(Line::Note));
    }
    lines.push(Line::Blank);
    lines
}

/// Word-wraps `s` at `width` cells (a word longer than the width is cut).
fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && text::width(&line) + 1 + text::width(word) > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.into_iter()
        .map(|l| text::truncate_end(&l, width))
        .collect()
}

/// Splits groups (their line counts `heights`, each with a trailing blank
/// line that is dropped at the end of a column) into at most `n` columns,
/// in order, minimising the tallest column. Returns each column's groups
/// as a range of indices.
fn pack(heights: &[usize], n: usize) -> Vec<std::ops::Range<usize>> {
    let n = n.max(1);
    let mut limit = heights.iter().copied().max().unwrap_or(0).saturating_sub(1);
    loop {
        let mut cols = Vec::new();
        let mut start = 0;
        let mut used = 0;
        for (i, h) in heights.iter().enumerate() {
            if (used + h).saturating_sub(1) > limit && i > start {
                cols.push(start..i);
                start = i;
                used = 0;
            }
            used += h;
        }
        cols.push(start..heights.len());
        if cols.len() <= n {
            return cols;
        }
        limit += 1;
    }
}

/// Lays the groups out for a screen `width` cells wide: one `Vec<Line>`
/// per column, and the column width.
fn layout(groups: &[HelpGroup], width: u16) -> (Vec<Vec<Line>>, u16) {
    let n = column_count(width);
    // The border and a 1-cell margin on each side.
    let usable = width.saturating_sub(4);
    let col_width = usable.saturating_sub(COLUMN_GAP * (n - 1)) / n;
    let per_group: Vec<Vec<Line>> = groups
        .iter()
        .map(|g| group_lines(g, usize::from(col_width)))
        .collect();
    // A continuation joins the previous block, without the blank line.
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    for (group, lines) in groups.iter().zip(per_group) {
        match blocks.last_mut() {
            Some(prev) if group.continues => {
                if prev.last() == Some(&Line::Blank) {
                    prev.pop();
                }
                prev.extend(lines);
            }
            _ => blocks.push(lines),
        }
    }
    let heights: Vec<usize> = blocks.iter().map(Vec::len).collect();
    let columns = pack(&heights, usize::from(n))
        .into_iter()
        .map(|range| {
            let mut lines: Vec<Line> = blocks[range].iter().flatten().cloned().collect();
            if lines.last() == Some(&Line::Blank) {
                lines.pop();
            }
            lines
        })
        .collect();
    (columns, col_width)
}

/// Wraps `items` into lines `width` cells wide: `(text, is_key)` pieces.
fn wrap_items(items: &[Item], width: usize) -> Vec<Vec<(String, bool)>> {
    let item_width = |item: Item| {
        item.iter().map(|(t, _)| text::width(t)).sum::<usize>() + item.len().saturating_sub(1)
    };
    let mut lines: Vec<Vec<(String, bool)>> = Vec::new();
    let mut line: Vec<(String, bool)> = Vec::new();
    let mut used = 0;
    for item in items {
        let w = item_width(item);
        if !line.is_empty() && used + 2 + w > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if !line.is_empty() {
            line.push(("  ".to_owned(), false));
            used += 2;
        }
        for (i, (t, key)) in item.iter().enumerate() {
            if i > 0 {
                line.push((" ".to_owned(), false));
            }
            line.push(((*t).to_owned(), *key));
        }
        used += w;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The footer lines (cheat sheet, then the go-to note) for `width` cells.
fn footer_lines(width: usize) -> Vec<Vec<(String, bool)>> {
    let mut lines = wrap_items(CHEAT_SHEET, width);
    lines.extend(wrap_items(GOTO_NOTE, width));
    lines
}

/// Group titles and footer labels: bold teal.
fn title_style(theme: &Theme) -> Style {
    theme.dialog_border(theme.teal).add_modifier(Modifier::BOLD)
}

/// Writes styled pieces from `x`, clipped at `right`.
fn put_pieces(buf: &mut Buffer, x: u16, y: u16, right: u16, pieces: &[(String, Style)]) {
    let mut x = x;
    for (t, style) in pieces {
        if x >= right {
            break;
        }
        let shown = text::truncate_end(t, usize::from(right - x));
        x = put(buf, x, y, &shown, *style);
    }
}

/// Draws one content line in a column `width` cells wide, with keys in a
/// column `key_width` wide.
fn draw_line(
    buf: &mut Buffer,
    (x, y): (u16, u16),
    width: u16,
    key_width: u16,
    line: &Line,
    theme: &Theme,
) {
    let right = x + width;
    match line {
        Line::Title(t) => {
            let style = title_style(theme);
            put_pieces(buf, x, y, right, &[((*t).to_owned(), style)]);
        }
        Line::Entry { keys, desc } => {
            let keys = text::truncate_end(keys, usize::from(key_width));
            put(buf, x, y, &keys, theme.hint_key());
            let dx = x + key_width + 2;
            if dx < right {
                let desc = text::truncate_end(desc, usize::from(right - dx));
                put(buf, dx, y, &desc, theme.hint());
            }
        }
        Line::Note(n) => put_pieces(buf, x, y, right, &[(n.clone(), theme.hint())]),
        Line::Blank => {}
    }
}

impl Component for Help {
    fn register_config_handler(&mut self, config: Config) -> color_eyre::Result<()> {
        *self = Help::new(config.keybindings);
        Ok(())
    }

    fn update(
        &mut self,
        action: &Action,
        state: &mut AppState,
    ) -> color_eyre::Result<Option<Action>> {
        if state.overlay != Some(Overlay::Help) {
            return Ok(None);
        }
        let page = self.page.max(1) as isize;
        match action {
            // `App` has just opened it.
            Action::Help => self.scroll = 0,
            Action::SelectNext | Action::MoveDown => self.scroll_by(1),
            Action::SelectPrev | Action::MoveUp => self.scroll_by(-1),
            Action::PageDown | Action::HalfPageDown => self.scroll_by(page),
            Action::PageUp | Action::HalfPageUp => self.scroll_by(-page),
            Action::FirstRow => self.scroll = 0,
            Action::LastRow => self.scroll = self.max_scroll,
            _ => {}
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let title = format!(" Help — tachy {} ", env!("CARGO_PKG_VERSION"));
        let block = Block::bordered()
            .style(theme.dialog())
            .border_style(theme.dialog_border(theme.teal))
            .title(title)
            .title_style(theme.dialog_title(theme.teal));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        if inner.width < 4 || inner.height < 2 {
            return Ok(());
        }
        let buf = frame.buffer_mut();
        let text_x = inner.x + 1;
        let text_w = inner.width - 2;

        // Footer at the bottom: the cheat sheet and the go-to note, their
        // labels in the group-title style.
        let footer = footer_lines(usize::from(text_w));
        let footer_h = (footer.len() as u16).min(inner.height - 1);
        let footer_y = inner.bottom() - footer_h;
        for (i, line) in footer.iter().enumerate().take(usize::from(footer_h)) {
            let pieces: Vec<(String, Style)> = line
                .iter()
                .enumerate()
                .map(|(j, (t, key))| {
                    let style = match (j, key) {
                        (0, _) => title_style(theme),
                        (_, true) => theme.hint_key(),
                        _ => theme.hint(),
                    };
                    (t.clone(), style)
                })
                .collect();
            put_pieces(buf, text_x, footer_y + i as u16, text_x + text_w, &pieces);
        }

        // Content.
        let content_h = usize::from(footer_y - inner.y);
        let (columns, col_w) = layout(&self.groups, area.width);
        let tallest = columns.iter().map(Vec::len).max().unwrap_or(0);
        self.page = content_h.max(1);
        self.max_scroll = tallest.saturating_sub(content_h);
        self.scroll = self.scroll.min(self.max_scroll);
        for (c, lines) in columns.iter().enumerate() {
            let x = text_x + c as u16 * (col_w + COLUMN_GAP);
            let key_w = lines
                .iter()
                .filter_map(|l| match l {
                    Line::Entry { keys, .. } => Some(text::width(keys)),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                .min(usize::from(col_w / MAX_KEY_SHARE)) as u16;
            for (row, line) in lines.iter().skip(self.scroll).take(content_h).enumerate() {
                draw_line(buf, (x, inner.y + row as u16), col_w, key_w, line, theme);
            }
        }

        // Bottom border: the help's own keys, and the position when it scrolls.
        let mut pieces: Vec<(String, Style)> = vec![(" ".to_owned(), theme.hint())];
        for (keys, label) in help_model::help_keys(&self.keymap) {
            pieces.push((keys, theme.hint_key()));
            pieces.push((format!(" {label}  "), theme.hint()));
        }
        if let Some((t, _)) = pieces.last_mut() {
            *t = format!("{} ", t.trim_end());
        }
        let y = area.bottom() - 1;
        put_pieces(buf, area.x + 2, y, area.right().saturating_sub(2), &pieces);
        if self.max_scroll > 0 {
            let last = (self.scroll + content_h).min(tallest);
            let pos = format!(" {}–{} of {} ", self.scroll + 1, last, tallest);
            let w = text::width(&pos) as u16;
            if area.width > w + 4 {
                put(buf, area.right() - 2 - w, y, &pos, theme.hint());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        cli::Cli, keymap::parse_key_chord, mode::KeyContext, settings::Settings,
        theme::ColorSupport,
    };

    fn state() -> AppState {
        let cli = Cli::try_parse_from(["tachy", "x.csv"]).unwrap();
        let settings = Settings::resolve(&cli, &Config::embedded()).unwrap();
        AppState::new(settings, ColorSupport::TrueColor)
    }

    fn help() -> Help {
        Help::new(Config::embedded().keybindings)
    }

    fn render(help: &mut Help, state: &AppState, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| help.draw(f, f.area(), state).unwrap())
            .unwrap();
        terminal
    }

    fn act(help: &mut Help, state: &mut AppState, action: Action) {
        help.update(&action, state).unwrap();
    }

    #[test]
    fn columns_by_width() {
        assert_eq!(column_count(80), 1);
        assert_eq!(column_count(99), 1);
        assert_eq!(column_count(100), 2);
        assert_eq!(column_count(159), 2);
        assert_eq!(column_count(160), 3);
    }

    #[test]
    fn packing_keeps_order_and_balances() {
        assert_eq!(pack(&[3, 3, 3], 3), [0..1, 1..2, 2..3]);
        assert_eq!(pack(&[5, 1, 1, 1, 1, 1], 2), [0..1, 1..6]);
        assert_eq!(pack(&[2, 2, 2, 2], 1), vec![(0..4)]);
        assert_eq!(pack(&[10, 1], 3), [0..1, 1..2]);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap_words("aa bb cc", 5), ["aa bb", "cc"]);
        assert!(wrap_items(CHEAT_SHEET, 60).len() > 1);
        for line in footer_lines(76) {
            let w: usize = line.iter().map(|(t, _)| text::width(t)).sum();
            assert!(w <= 76, "{line:?}");
        }
    }

    #[test]
    fn open_and_close_keep_the_pill() {
        let mut s = state();
        open(&mut s);
        assert_eq!(s.overlay, Some(Overlay::Help));
        assert_eq!(s.mode, Mode::Dialog);
        assert_eq!(s.visible_mode(), Mode::Normal);
        assert_eq!(s.key_context(), KeyContext::Help);
        close(&mut s);
        assert_eq!(s.overlay, None);
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn scrolling_is_clamped() {
        let mut s = state();
        open(&mut s);
        let mut h = help();
        render(&mut h, &s, 80, 24);
        assert!(h.max_scroll > 0);
        act(&mut h, &mut s, Action::SelectPrev);
        assert_eq!(h.scroll, 0);
        act(&mut h, &mut s, Action::SelectNext);
        assert_eq!(h.scroll, 1);
        act(&mut h, &mut s, Action::LastRow);
        assert_eq!(h.scroll, h.max_scroll);
        act(&mut h, &mut s, Action::PageDown);
        assert_eq!(h.scroll, h.max_scroll);
        act(&mut h, &mut s, Action::FirstRow);
        assert_eq!(h.scroll, 0);
        // Closed: actions are ignored.
        close(&mut s);
        act(&mut h, &mut s, Action::SelectNext);
        assert_eq!(h.scroll, 0);
    }

    #[test]
    fn fits_without_scrolling_when_wide() {
        let s = state();
        let mut h = help();
        render(&mut h, &s, 200, 60);
        assert_eq!(h.max_scroll, 0);
    }

    #[test]
    fn snapshots() {
        let mut s = state();
        open(&mut s);
        let mut h = help();
        for (w, ht) in [(160, 48), (120, 40)] {
            let t = render(&mut h, &s, w, ht);
            insta::assert_snapshot!(format!("help_{w}x{ht}"), t.backend());
        }
        let t = render(&mut h, &s, 80, 24);
        insta::assert_snapshot!("help_80x24_top", t.backend());
        act(&mut h, &mut s, Action::LastRow);
        let t = render(&mut h, &s, 80, 24);
        insta::assert_snapshot!("help_80x24_bottom", t.backend());
    }

    #[test]
    fn remapped_quit_is_shown() {
        let mut keymap = Config::embedded().keybindings;
        keymap
            .0
            .get_mut(&KeyContext::Normal)
            .unwrap()
            .insert(parse_key_chord("Q").unwrap(), Action::Quit);
        let mut h = Help::new(keymap);
        let t = render(&mut h, &state(), 160, 48);
        let screen = format!("{}", t.backend());
        assert!(screen.contains("Q q ctrl-c ctrl-q"), "{screen}");
    }
}
