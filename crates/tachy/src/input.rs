//! A reusable single-line editor (M4-04 `LineEdit`): the `open ›` prompt
//! (M1-08), the go-to dialog (M2-03), the column chooser filter (M3-04), the
//! filter and search bars (M4-04, M4-05) and the export path (M6-01).
//!
//! Only editing keys are handled here. Keys bound in the input's key context
//! (`Enter`, `Esc`, `Tab`, `↑`/`↓`, …) are resolved by the keymap first and
//! never reach the editor.
//!
//! The cursor is a byte offset on a grapheme boundary: `←`/`→`,
//! `Backspace` and `Delete` move and delete whole extended grapheme
//! clusters, so a combining accent or an emoji with modifiers is one step.
//! Horizontal scrolling ([`LineEdit::window`]) is measured in display cells.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tachy_core::text;
use unicode_segmentation::UnicodeSegmentation;

/// The text, the cursor (a byte offset on a grapheme boundary) and the
/// horizontal scroll offset (a byte offset of the first shown grapheme).
#[derive(Debug, Clone, Default)]
pub struct LineEdit {
    text: String,
    cursor: usize,
    /// Updated when drawing ([`LineEdit::window`]), so it is a `Cell`: the
    /// draw path only has `&AppState`.
    scroll: Cell<usize>,
}

/// The name used by the callers written before M4-04.
pub type LineInput = LineEdit;

impl PartialEq for LineEdit {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.cursor == other.cursor
    }
}

impl Eq for LineEdit {}

/// Characters that end a word for `ctrl-w` / `alt-backspace`: whitespace and
/// path separators (`/`, and `\` on Windows), so a path loses one component
/// at a time.
fn is_word_separator(c: char) -> bool {
    c.is_whitespace() || crate::path_complete::is_separator(c)
}

impl LineEdit {
    /// An editor holding `text`, the cursor at the end.
    pub fn with_text(text: impl Into<String>) -> Self {
        let mut e = LineEdit::default();
        e.set(text);
        e
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the text and puts the cursor at the end.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.scroll.set(0);
    }

    /// Replaces `range` (byte offsets on char boundaries) with `with` and
    /// puts the cursor after it (Tab completion).
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, with: &str) {
        let end = range.start + with.len();
        self.text.replace_range(range, with);
        self.cursor = end.min(self.text.len());
    }

    /// Inserts `s` at the cursor (typing). Control characters (a pasted
    /// newline in a path) are dropped.
    pub fn insert(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Bracketed paste into a query (`Event::Paste`): line breaks become
    /// spaces, other control characters are dropped.
    pub fn paste(&mut self, s: &str) {
        let clean: String = s
            .replace("\r\n", " ")
            .chars()
            .filter_map(|c| match c {
                '\n' | '\r' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect();
        self.insert(&clean);
    }

    /// Start of the grapheme before the cursor.
    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// End of the grapheme after the cursor.
    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    /// Start of the word before the cursor, skipping the separators right
    /// before it.
    fn word_start(&self) -> usize {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end_matches(is_word_separator);
        trimmed
            .char_indices()
            .rev()
            .find(|&(_, c)| is_word_separator(c))
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    fn delete_word(&mut self) {
        let start = self.word_start();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Applies an editing key. Returns whether it was an editing key (and so
    /// was used), even when it changed nothing (`Backspace` at the start).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut buf = [0; 4];
                self.insert(c.encode_utf8(&mut buf));
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.text.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if alt || ctrl => self.delete_word(),
            KeyCode::Backspace => {
                let start = self.prev_boundary();
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Delete => {
                let end = self.next_boundary();
                self.text.drain(self.cursor..end);
            }
            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }

    /// Horizontal scrolling for a field `width` cells wide, one of which is
    /// kept for the block cursor at the end: returns the byte offset of the
    /// first shown grapheme. The offset only moves when the cursor would
    /// leave the field (or when there is room again on the left), so the
    /// text doesn't jump on every key.
    pub fn window(&self, width: usize) -> usize {
        let width = width.max(1);
        let mut scroll = self.scroll.get().min(self.cursor);
        if !self.text.is_char_boundary(scroll) {
            scroll = 0;
        }
        // The cursor cell must fit: width(scroll..cursor) + 1 <= width.
        while scroll < self.cursor && text::width(&self.text[scroll..self.cursor]) + 1 > width {
            scroll += self.text[scroll..]
                .graphemes(true)
                .next()
                .map_or(1, str::len);
        }
        // Scroll back while the whole tail (and the cursor cell) still fits
        // with the grapheme on the left: after deletions.
        while let Some((i, _)) = self.text[..scroll].grapheme_indices(true).next_back() {
            if text::width(&self.text[i..]) < width {
                scroll = i;
            } else {
                break;
            }
        }
        self.scroll.set(scroll);
        scroll
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn editing() {
        let mut i = LineInput::default();
        for c in "héllo".chars() {
            assert!(i.handle_key(key(KeyCode::Char(c))));
        }
        assert_eq!(i.text(), "héllo");
        i.handle_key(key(KeyCode::Left));
        i.handle_key(key(KeyCode::Left));
        i.handle_key(key(KeyCode::Left));
        i.handle_key(key(KeyCode::Left));
        assert_eq!(i.cursor(), 1);
        i.handle_key(key(KeyCode::Delete));
        assert_eq!(i.text(), "hllo");
        i.handle_key(key(KeyCode::Char('é')));
        assert_eq!(i.text(), "héllo");
        i.handle_key(key(KeyCode::Backspace));
        assert_eq!(i.text(), "hllo");
        i.handle_key(key(KeyCode::End));
        i.handle_key(key(KeyCode::Backspace));
        assert_eq!(i.text(), "hll");
        // Control keys that aren't editing keys are not handled.
        assert!(!i.handle_key(ctrl('x')));
        assert_eq!(i.text(), "hll");
        // Editing keys at the ends are used but change nothing.
        i.handle_key(key(KeyCode::Home));
        assert!(i.handle_key(key(KeyCode::Backspace)));
        i.handle_key(key(KeyCode::End));
        assert!(i.handle_key(key(KeyCode::Delete)));
        assert_eq!(i.text(), "hll");
    }

    #[test]
    fn arrows_and_deletion_step_over_whole_graphemes() {
        // `e` + combining acute, a flag (two regional indicators) and a
        // family emoji joined with ZWJs: one grapheme each.
        let s = "ae\u{301}🇩🇪👨‍👩‍👧z";
        let mut i = LineEdit::with_text(s);
        i.handle_key(key(KeyCode::Left));
        assert_eq!(&s[i.cursor()..], "z");
        i.handle_key(key(KeyCode::Left));
        assert_eq!(&s[i.cursor()..], "👨‍👩‍👧z");
        i.handle_key(key(KeyCode::Left));
        assert_eq!(&s[i.cursor()..], "🇩🇪👨‍👩‍👧z");
        i.handle_key(key(KeyCode::Left));
        assert_eq!(&s[i.cursor()..], "e\u{301}🇩🇪👨‍👩‍👧z");
        i.handle_key(key(KeyCode::Right));
        assert_eq!(&s[i.cursor()..], "🇩🇪👨‍👩‍👧z");
        i.handle_key(key(KeyCode::Backspace));
        assert_eq!(i.text(), "a🇩🇪👨‍👩‍👧z");
        i.handle_key(key(KeyCode::Delete));
        assert_eq!(i.text(), "a👨‍👩‍👧z");
        i.handle_key(key(KeyCode::Delete));
        assert_eq!(i.text(), "az");
        assert_eq!(i.cursor(), 1);
    }

    #[test]
    fn word_and_line_deletion() {
        let mut i = LineInput::default();
        i.set("~/data/orders.csv");
        i.handle_key(ctrl('w'));
        assert_eq!(i.text(), "~/data/");
        i.handle_key(ctrl('w'));
        assert_eq!(i.text(), "~/");
        i.set("abc def");
        i.handle_key(ctrl('a'));
        i.handle_key(ctrl('k'));
        assert_eq!(i.text(), "");
        i.set("abc def");
        i.handle_key(key(KeyCode::Left));
        i.handle_key(ctrl('u'));
        assert_eq!((i.text(), i.cursor()), ("f", 0));
        i.set("abc def");
        i.handle_key(ctrl('a'));
        i.handle_key(ctrl('e'));
        assert_eq!(i.cursor(), 7);
    }

    #[test]
    fn word_deletion_stops_at_backslashes_on_windows_only() {
        let mut i = LineInput::default();
        i.set("data\\sub\\orders.csv");
        i.handle_key(ctrl('w'));
        if cfg!(windows) {
            assert_eq!(i.text(), "data\\sub\\");
        } else {
            // A file-name character on Unix: the whole word goes.
            assert_eq!(i.text(), "");
        }
    }
    #[test]
    fn alt_backspace_deletes_the_previous_word() {
        let mut i = LineEdit::with_text("price > 10  ");
        i.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(i.text(), "price > ");
        i.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(i.text(), "price ");
        i.handle_key(ctrl('w'));
        assert_eq!(i.text(), "");
        // Mid-text: only the part before the cursor goes.
        let mut i = LineEdit::with_text("a bcd");
        i.handle_key(key(KeyCode::Left));
        i.handle_key(ctrl('w'));
        assert_eq!((i.text(), i.cursor()), ("a d", 2));
    }

    #[test]
    fn paste_drops_control_characters() {
        let mut i = LineInput::default();
        i.insert("a.csv\n");
        assert_eq!(i.text(), "a.csv");
    }

    #[test]
    fn paste_turns_newlines_into_spaces() {
        let mut i = LineEdit::with_text("x");
        i.handle_key(key(KeyCode::Home));
        i.paste("price > 1\r\n&& country\n== \"DE\"\t");
        assert_eq!(i.text(), "price > 1 && country == \"DE\"x");
        assert_eq!(i.cursor(), i.text().len() - 1);
    }

    #[test]
    fn replace_range_moves_the_cursor_after_the_replacement() {
        let mut i = LineEdit::with_text("pr > 1");
        i.replace_range(0..2, "price");
        assert_eq!((i.text(), i.cursor()), ("price > 1", 5));
    }

    #[test]
    fn window_keeps_the_cursor_visible() {
        let mut i = LineEdit::with_text("0123456789");
        // 10 chars + the cursor cell in 6 cells: scrolled by 5.
        assert_eq!(i.window(6), 5);
        // Moving left within the window doesn't scroll.
        for _ in 0..3 {
            i.handle_key(key(KeyCode::Left));
        }
        assert_eq!(i.window(6), 5);
        // Past the left edge, it follows the cursor.
        for _ in 0..3 {
            i.handle_key(key(KeyCode::Left));
        }
        assert_eq!(i.cursor(), 4);
        assert_eq!(i.window(6), 4);
        i.handle_key(key(KeyCode::Home));
        assert_eq!(i.window(6), 0);
        // Wide characters count two cells.
        let i = LineEdit::with_text("日本語のテキスト");
        let s = i.window(7);
        let shown = text::width(&i.text()[s..]);
        assert!((5..7).contains(&shown), "{shown}");
        // Room again after deleting: scrolls back.
        let mut i = LineEdit::with_text("0123456789");
        assert_eq!(i.window(6), 5);
        for _ in 0..8 {
            i.handle_key(key(KeyCode::Backspace));
        }
        assert_eq!(i.window(6), 0);
    }
}
