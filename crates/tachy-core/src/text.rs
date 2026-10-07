//! Display-width helpers: fitting, truncating and measuring text in terminal
//! cells (spec §11.3).
//!
//! Widths follow the same rule as the terminal renderer: a string is split
//! into extended grapheme clusters and each cluster counts
//! `UnicodeWidthStr::width` cells, so wide characters (CJK, emoji) count as
//! 2. Nothing here depends on ratatui, so the table, the inspector and the
//! export code share one implementation.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The ellipsis that ends a truncated value. One cell wide.
pub const ELLIPSIS: &str = "…";

/// Horizontal alignment of a value in its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// Text and everything else (§11.3).
    #[default]
    Left,
    /// Numeric columns (`i64`, `f64`, §11.3).
    Right,
}

/// The result of [`fit`]: what to draw, cell by cell, left to right.
///
/// `pad_left` spaces, then `text`, then `pad_right` spaces, then `…` when
/// `truncated`. The total is always exactly the requested width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FittedText<'a> {
    /// The visible prefix of the input (whole graphemes only).
    pub text: &'a str,
    /// Width of `text` in cells.
    pub text_width: u16,
    /// Spaces before `text` (right alignment).
    pub pad_left: u16,
    /// Spaces after `text`. When truncated, they sit between the text and
    /// the ellipsis: a wide character that doesn't fit is replaced by a
    /// space, never split.
    pub pad_right: u16,
    /// The value was cut; draw `…` in the last cell.
    pub truncated: bool,
}

impl FittedText<'_> {
    /// Total width in cells: always the width passed to [`fit`].
    pub fn width(&self) -> u16 {
        self.pad_left + self.text_width + self.pad_right + u16::from(self.truncated)
    }

    /// The fitted text as one string, padding and ellipsis included.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + usize::from(self.width()) + 2);
        out.extend(std::iter::repeat_n(' ', usize::from(self.pad_left)));
        out.push_str(self.text);
        out.extend(std::iter::repeat_n(' ', usize::from(self.pad_right)));
        if self.truncated {
            out.push_str(ELLIPSIS);
        }
        out
    }
}

/// Display width of `s` in cells, grapheme by grapheme.
pub fn width(s: &str) -> usize {
    s.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Fits `s` into exactly `width` cells (§11.3).
///
/// - Fits: padded on the right (`Left`) or the left (`Right`).
/// - Too wide: the longest prefix of whole graphemes that fits in
///   `width − 1` cells, padded with spaces, then `…`. A wide character is
///   never split: if only 1 cell is left for it, a space is drawn instead.
/// - `width == 0`: nothing.
pub fn fit(s: &str, width: u16, align: Align) -> FittedText<'_> {
    let full = self::width(s);
    if full <= usize::from(width) {
        let gap = width - full as u16;
        let (pad_left, pad_right) = match align {
            Align::Left => (0, gap),
            Align::Right => (gap, 0),
        };
        return FittedText {
            text: s,
            text_width: full as u16,
            pad_left,
            pad_right,
            truncated: false,
        };
    }
    if width == 0 {
        return FittedText {
            text: "",
            text_width: 0,
            pad_left: 0,
            pad_right: 0,
            truncated: false,
        };
    }
    let budget = usize::from(width) - 1;
    let (end, used) = prefix_within(s, budget);
    FittedText {
        text: &s[..end],
        text_width: used as u16,
        pad_left: 0,
        pad_right: (budget - used) as u16,
        truncated: true,
    }
}

/// Byte length and width of the longest prefix of whole graphemes of `s`
/// that is at most `max` cells wide.
pub fn prefix_within(s: &str, max: usize) -> (usize, usize) {
    let mut used = 0;
    let mut end = 0;
    for (i, g) in s.grapheme_indices(true) {
        let w = g.width();
        if used + w > max {
            break;
        }
        used += w;
        end = i + g.len();
    }
    (end, used)
}

/// `s` cut to at most `max` cells, ending in `…` when cut. Unlike [`fit`],
/// no padding is added.
pub fn truncate_end(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let (end, _) = prefix_within(s, max - 1);
    format!("{}{ELLIPSIS}", &s[..end])
}

/// `s` cut in the middle to at most `max` cells: `orders_2…03.csv`. The
/// start gets the extra cell when the budget is odd. Used for file names
/// (top bar, status line), where both ends carry meaning.
pub fn truncate_middle(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    let head_max = budget.div_ceil(2);
    let (head_end, head_w) = prefix_within(s, head_max);
    let tail_max = budget - head_w;
    // The longest suffix of whole graphemes within `tail_max`.
    let mut tail_start = s.len();
    let mut used = 0;
    for (i, g) in s.grapheme_indices(true).rev() {
        let w = g.width();
        if used + w > tail_max || i < head_end {
            break;
        }
        used += w;
        tail_start = i;
    }
    format!("{}{ELLIPSIS}{}", &s[..head_end], &s[tail_start..])
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn rendered(s: &str, w: u16, align: Align) -> String {
        let f = fit(s, w, align);
        assert_eq!(f.width(), w, "{s:?} in {w}");
        assert_eq!(width(&f.render()), usize::from(w), "{s:?} in {w}");
        f.render()
    }

    #[test]
    fn ascii() {
        assert_eq!(rendered("hello", 8, Align::Left), "hello   ");
        assert_eq!(rendered("hello", 8, Align::Right), "   hello");
        assert_eq!(rendered("hello", 5, Align::Left), "hello");
        assert_eq!(rendered("hello", 4, Align::Left), "hel…");
        assert_eq!(rendered("hello", 4, Align::Right), "hel…");
        assert_eq!(rendered("", 3, Align::Left), "   ");
        let f = fit("hello", 4, Align::Left);
        assert!(f.truncated);
        assert_eq!(f.text, "hel");
        assert!(!fit("hello", 5, Align::Left).truncated);
    }

    #[test]
    fn exact_fit_has_no_padding_and_no_ellipsis() {
        let f = fit("日本", 4, Align::Right);
        assert_eq!((f.pad_left, f.pad_right, f.truncated), (0, 0, false));
        assert_eq!(f.text, "日本");
    }

    #[test]
    fn cjk_at_odd_widths() {
        // 日本語 is 6 cells.
        assert_eq!(rendered("日本語", 6, Align::Left), "日本語");
        assert_eq!(rendered("日本語", 7, Align::Left), "日本語 ");
        assert_eq!(rendered("日本語", 7, Align::Right), " 日本語");
        // 4 cells for text: 日本 fits exactly.
        assert_eq!(rendered("日本語", 5, Align::Left), "日本…");
        // 3 cells for text: 日 + a space, never half of 本.
        assert_eq!(rendered("日本語", 4, Align::Left), "日 …");
        let f = fit("日本語", 4, Align::Left);
        assert_eq!((f.text, f.text_width, f.pad_right), ("日", 2, 1));
        assert_eq!(rendered("日本語", 2, Align::Left), " …");
    }

    #[test]
    fn emoji() {
        assert_eq!(width("🦀"), 2);
        assert_eq!(rendered("🦀🦀", 4, Align::Left), "🦀🦀");
        assert_eq!(rendered("🦀🦀", 3, Align::Left), "🦀…");
        assert_eq!(rendered("a🦀b", 3, Align::Left), "a …");
        // A family emoji is one grapheme: kept whole or dropped whole.
        let family = "👨\u{200d}👩\u{200d}👧";
        let f = fit(family, 1, Align::Left);
        assert_eq!(f.text, "");
        assert!(f.truncated);
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let s = "e\u{301}e\u{301}e\u{301}"; // ééé, 3 cells
        assert_eq!(width(s), 3);
        let f = fit(s, 2, Align::Left);
        assert_eq!(f.text, "e\u{301}");
    }

    #[test]
    fn width_zero_and_one() {
        let f = fit("hello", 0, Align::Left);
        assert_eq!(f.width(), 0);
        assert_eq!(f.render(), "");
        assert_eq!(rendered("hello", 1, Align::Left), "…");
        assert_eq!(rendered("h", 1, Align::Left), "h");
        assert_eq!(rendered("", 0, Align::Left), "");
        assert_eq!(rendered("日", 1, Align::Left), "…");
    }

    #[test]
    fn truncation_helpers() {
        assert_eq!(truncate_end("hello", 5), "hello");
        assert_eq!(truncate_end("hello", 4), "hel…");
        assert_eq!(truncate_end("hello", 0), "");
        assert_eq!(truncate_end("日本語", 4), "日…");

        assert_eq!(truncate_middle("orders.csv", 24), "orders.csv");
        assert_eq!(
            truncate_middle("orders_2024_full_export_03.csv", 24),
            "orders_2024_…port_03.csv"
        );
        assert_eq!(truncate_middle("abcdefgh", 5), "ab…gh");
        assert_eq!(truncate_middle("abcdefgh", 4), "ab…h");
        assert_eq!(truncate_middle("abcdefgh", 1), "…");
        assert_eq!(truncate_middle("abcdefgh", 0), "");
        assert_eq!(width(&truncate_middle("日本語日本語", 7)), 7);
    }
}
