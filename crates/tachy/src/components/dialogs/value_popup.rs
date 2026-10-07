//! The full-value popup (spec §11.4, §13, M3-03): `Enter` on a field of the
//! focused inspector's RECORD section. A centred teal dialog,
//! `min(100, w − 4)` × `min(30, h − 4)`, titled with the field name. The value
//! is soft-wrapped at the width, control characters escaped (dim), and a
//! line break follows each escaped `\n`. Values over 1 MiB show their first
//! 1 MiB and a `showing 1.0 MB of 312.4 MB` footer.

use ratatui::{Frame, layout::Rect};
use tachy_core::{parse::display_segments, size::format_size, text};

use super::dialog_frame;
use crate::{
    components::{layout::centered_rect, put},
    theme::Theme,
};

/// At most this much of a value is shown.
pub const MAX_BYTES: usize = 1 << 20;
const FOOTER: &str = "j/k scroll · Esc close";

/// One wrapped line: `(text, escaped)` pieces.
pub type WrappedLine = Vec<(String, bool)>;

/// The popup's state. `App` keeps it in `AppState::value_popup`.
#[derive(Debug, Clone, Default)]
pub struct ValuePopupState {
    /// The field name.
    pub title: String,
    /// The value, cut to [`MAX_BYTES`] (on a char boundary).
    pub value: String,
    /// Bytes of the whole value.
    pub total_bytes: u64,
    /// First wrapped line shown.
    pub scroll: usize,
    /// `value` wrapped at `wrap_width`.
    pub lines: Vec<WrappedLine>,
    pub wrap_width: u16,
}

impl ValuePopupState {
    /// A popup for `value` (decoded), wrapped for a screen of `area`.
    pub fn new(title: String, value: &str, area: Rect) -> Self {
        let total_bytes = value.len() as u64;
        let mut end = value.len().min(MAX_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let mut s = ValuePopupState {
            title,
            value: value[..end].to_owned(),
            total_bytes,
            ..ValuePopupState::default()
        };
        s.rewrap(area);
        s
    }

    /// Whether only part of the value is shown.
    pub fn truncated(&self) -> bool {
        (self.value.len() as u64) < self.total_bytes
    }

    /// Re-wraps for a new screen size (resize).
    pub fn rewrap(&mut self, area: Rect) {
        let width = text_rect(area).width.max(1);
        if width != self.wrap_width {
            self.lines = wrap(&self.value, usize::from(width));
            self.wrap_width = width;
        }
        self.scroll = self.scroll.min(self.max_scroll(area));
    }

    /// Text rows shown at `area`'s size.
    pub fn page(area: Rect) -> usize {
        usize::from(text_rect(area).height.max(1))
    }

    fn max_scroll(&self, area: Rect) -> usize {
        self.lines.len().saturating_sub(Self::page(area))
    }

    /// Scrolls by `delta` lines, clamped.
    pub fn scroll_by(&mut self, delta: isize, area: Rect) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll(area));
    }

    /// `Home` / `G`.
    pub fn scroll_to(&mut self, end: bool, area: Rect) {
        self.scroll = if end { self.max_scroll(area) } else { 0 };
    }
}

/// The dialog rect for a screen of `area`.
pub fn popup_rect(area: Rect) -> Rect {
    let w = 100.min(area.width.saturating_sub(4));
    let h = 30.min(area.height.saturating_sub(4));
    centered_rect(w, h, area)
}

/// Where the value's text goes: inside the border, a 1-cell margin left and
/// right, the last inner row kept for the footer.
fn text_rect(area: Rect) -> Rect {
    let r = popup_rect(area);
    Rect::new(
        r.x + 2,
        r.y + 1,
        r.width.saturating_sub(4),
        r.height.saturating_sub(3),
    )
}

/// Soft-wraps `value` at `width` cells, escaping control characters. A line
/// break follows each `\n` (shown escaped).
pub fn wrap(value: &str, width: usize) -> Vec<WrappedLine> {
    let mut lines = Vec::new();
    let mut line: WrappedLine = Vec::new();
    let mut used = 0;
    let push = |line: &mut WrappedLine, s: &str, escaped: bool| match line.last_mut() {
        Some((t, e)) if *e == escaped => t.push_str(s),
        _ => line.push((s.to_owned(), escaped)),
    };
    let mut parts = value.split('\n').peekable();
    while let Some(part) = parts.next() {
        let newline = parts.peek().is_some();
        let segments = display_segments(part);
        let tail = newline.then(|| ("\\n".to_owned(), true));
        for (seg_text, escaped) in segments
            .iter()
            .map(|s| (s.text.to_string(), s.escaped))
            .chain(tail)
        {
            for c in seg_text.chars() {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if used + w > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                let mut buf = [0; 4];
                push(&mut line, c.encode_utf8(&mut buf), escaped);
                used += w;
            }
        }
        if newline {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

pub fn draw(frame: &mut Frame, area: Rect, p: &ValuePopupState, theme: &Theme) {
    let rect = popup_rect(area);
    let inner = dialog_frame(frame, rect, &p.title, theme.teal, theme);
    if inner.height < 2 || inner.width < 4 {
        return;
    }
    let body = text_rect(area);
    let buf = frame.buffer_mut();
    let base = theme.dialog();
    let escaped = base.patch(theme.control_char());
    for (i, line) in p
        .lines
        .iter()
        .skip(p.scroll)
        .take(usize::from(body.height))
        .enumerate()
    {
        let y = body.y + i as u16;
        let mut x = body.x;
        for (s, esc) in line {
            x = put(buf, x, y, s, if *esc { escaped } else { base });
        }
    }
    let y = inner.bottom() - 1;
    put(buf, body.x, y, FOOTER, base.patch(theme.hint()));
    let mut right = String::new();
    if p.truncated() {
        right = format!(
            "showing {} of {}",
            format_size(p.value.len() as u64),
            format_size(p.total_bytes)
        );
    } else if p.lines.len() > usize::from(body.height) {
        right = format!(
            "{}–{} / {} lines",
            p.scroll + 1,
            (p.scroll + usize::from(body.height)).min(p.lines.len()),
            p.lines.len()
        );
    }
    if !right.is_empty() {
        let w = text::width(&right) as u16;
        put(
            buf,
            body.right().saturating_sub(w),
            y,
            &right,
            base.patch(theme.dim()),
        );
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn plain(lines: &[WrappedLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|(s, _)| s.as_str()).collect())
            .collect()
    }

    #[test]
    fn wraps_at_the_width_and_after_newlines() {
        assert_eq!(plain(&wrap("abcdefgh", 3)), ["abc", "def", "gh"]);
        assert_eq!(plain(&wrap("ab\ncd", 10)), ["ab\\n", "cd"]);
        assert_eq!(plain(&wrap("", 10)), [""]);
        // Wide characters are not split.
        assert_eq!(plain(&wrap("日本語", 5)), ["日本", "語"]);
        // Escapes are marked.
        let l = wrap("a\tb", 10);
        assert_eq!(
            l[0],
            vec![
                ("a".to_owned(), false),
                ("\\t".to_owned(), true),
                ("b".to_owned(), false)
            ]
        );
    }

    #[test]
    fn big_values_are_cut_at_one_mib() {
        let big = "x".repeat(MAX_BYTES + 10);
        let p = ValuePopupState::new("v".into(), &big, Rect::new(0, 0, 120, 40));
        assert!(p.truncated());
        assert_eq!(p.value.len(), MAX_BYTES);
    }

    #[test]
    fn scrolling_is_clamped() {
        let area = Rect::new(0, 0, 120, 40);
        let v = "y".repeat(96 * 100);
        let mut p = ValuePopupState::new("v".into(), &v, area);
        assert_eq!(p.wrap_width, 96);
        assert_eq!(p.lines.len(), 100);
        let page = ValuePopupState::page(area);
        assert_eq!(page, 27);
        p.scroll_to(true, area);
        assert_eq!(p.scroll, 100 - page);
        p.scroll_by(5, area);
        assert_eq!(p.scroll, 100 - page);
        p.scroll_by(-1000, area);
        assert_eq!(p.scroll, 0);
    }
}
