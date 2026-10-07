//! The Detected format dialog (spec §2.1, §12.3, M2-04, D3).
//!
//! Opens over the table after a file opens (unless the dialect came from
//! the command line, `--yes`, or `sniff.confirm: false`). Every change applies
//! live, debounced by 150 ms, so the table behind re-parses; the preview
//! re-highlights at once. `Enter` keeps the edits; `Esc` reverts to the
//! detected dialect (re-indexing only if the applied dialect differs).

use std::time::{Duration, Instant};

use ratatui::{Frame, layout::Rect, style::Style};
use tachy_core::{
    dialect::{Dialect, EscapeStyle, LineEnding},
    parse::{decode_field, display_segments},
    source::Source,
    text,
};

use super::dialog_frame;
use crate::{
    components::{layout::centered_rect, put},
    state::AppState,
    tab::TabId,
};

/// Height of the dialog; the width is `min(90, area.width − 4)`.
pub const HEIGHT: u16 = 15;
const MAX_WIDTH: u16 = 90;
/// A change applies once no other key came for this long.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// Raw lines shown in the preview.
const PREVIEW_LINES: usize = 3;
const LABEL_WIDTH: usize = 17;
const FOOTER: &str = "Enter accept · Esc revert · d delimiter · h header · q quoting · r raw";

/// The delimiter cycle of `d`.
const DELIMITERS: [u8; 6] = *b",\t|;: ";

/// The dialog's state. `App` keeps it in `AppState::detected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedFormatState {
    pub tab: TabId,
    /// The dialect being edited (shown, and applied after the debounce).
    pub current: Dialect,
    /// What `Esc` reverts to (D3): pure detection with the CLI overrides
    /// applied. Values that differ from it are drawn amber with a `*`.
    pub detected: Dialect,
    /// When the last edit applies (debounce); `None` when applied.
    pub apply_at: Option<Instant>,
}

impl DetectedFormatState {
    pub fn new(tab: TabId, current: Dialect, detected: Dialect) -> Self {
        DetectedFormatState {
            tab,
            current,
            detected,
            apply_at: None,
        }
    }

    /// `d`: the next delimiter.
    pub fn cycle_delimiter(&mut self, now: Instant) {
        self.current.delimiter = cycle_delimiter(self.current.delimiter, self.detected.delimiter);
        self.apply_at = Some(now + DEBOUNCE);
    }

    /// `h`: header yes ↔ no.
    pub fn toggle_header(&mut self, now: Instant) {
        self.current.header = !self.current.header;
        self.apply_at = Some(now + DEBOUNCE);
    }

    /// `q`: `"` → `'` → none → `"`.
    pub fn cycle_quote(&mut self, now: Instant) {
        self.current.quote = cycle_quote(self.current.quote);
        self.apply_at = Some(now + DEBOUNCE);
    }

    /// The edit is due: returns the dialect to apply and clears the timer.
    pub fn due(&mut self, now: Instant) -> Option<Dialect> {
        match self.apply_at {
            Some(at) if now >= at => {
                self.apply_at = None;
                Some(self.current)
            }
            _ => None,
        }
    }
}

/// `,` → `\t` → `|` → `;` → `:` → space → `,`. A detected delimiter
/// outside that list appears once in the cycle, first (where cycling starts
/// from), then the list follows.
pub fn cycle_delimiter(current: u8, detected: u8) -> u8 {
    let mut cycle: Vec<u8> = Vec::with_capacity(DELIMITERS.len() + 1);
    if !DELIMITERS.contains(&detected) {
        cycle.push(detected);
    }
    cycle.extend_from_slice(&DELIMITERS);
    match cycle.iter().position(|&d| d == current) {
        Some(i) => cycle[(i + 1) % cycle.len()],
        None => cycle[0],
    }
}

/// `"` → `'` → none → `"`.
pub fn cycle_quote(q: Option<u8>) -> Option<u8> {
    match q {
        Some(b'"') => Some(b'\''),
        Some(b'\'') => None,
        _ => Some(b'"'),
    }
}

/// `, (comma)`, `\t (tab)`, `0x1f`.
pub fn delimiter_name(b: u8) -> String {
    match b {
        b',' => ", (comma)".to_owned(),
        b'\t' => "\\t (tab)".to_owned(),
        b'|' => "| (pipe)".to_owned(),
        b';' => "; (semicolon)".to_owned(),
        b':' => ": (colon)".to_owned(),
        b' ' => "  (space)".to_owned(),
        0x21..=0x7e => (b as char).to_string(),
        _ => format!("0x{b:02x}"),
    }
}

fn quote_name(q: Option<u8>) -> String {
    match q {
        Some(q @ 0x21..=0x7e) => (q as char).to_string(),
        Some(q) => format!("0x{q:02x}"),
        None => "none".to_owned(),
    }
}

/// One piece of a preview line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPiece {
    pub text: String,
    /// A delimiter outside quotes (highlighted).
    pub delimiter: bool,
    /// An escaped control character (dim).
    pub escaped: bool,
}

/// The first [`PREVIEW_LINES`] raw lines of `src` (after the BOM, header
/// included), split into pieces: every byte equal to `d.delimiter` outside
/// quotes is its own highlighted piece. The quote state carries across lines
/// (a quoted newline). Line terminators are not shown.
pub fn preview(src: &Source, d: &Dialect) -> Vec<Vec<PreviewPiece>> {
    let bytes = src.bytes();
    let enc = d.encoding;
    let mut pos = enc.bom_len_in(bytes);
    let mut in_quotes = false;
    let mut lines = Vec::new();
    while pos < bytes.len() && lines.len() < PREVIEW_LINES {
        let end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| pos + i);
        let mut line = &bytes[pos..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        // Cap a huge line: the dialog shows ~90 cells.
        let line = &line[..line.len().min(4096)];
        let mut pieces = Vec::new();
        let mut start = 0;
        let mut i = 0;
        let push_text = |pieces: &mut Vec<PreviewPiece>, raw: &[u8]| {
            if raw.is_empty() {
                return;
            }
            let decoded = decode_field(raw, enc);
            for seg in display_segments(&decoded) {
                pieces.push(PreviewPiece {
                    text: seg.text.into_owned(),
                    delimiter: false,
                    escaped: seg.escaped,
                });
            }
        };
        while i < line.len() {
            let b = line[i];
            if in_quotes && d.escape == EscapeStyle::Backslash && b == b'\\' {
                i += 2;
                continue;
            }
            if Some(b) == d.quote {
                in_quotes = !in_quotes;
            } else if b == d.delimiter && !in_quotes {
                push_text(&mut pieces, &line[start..i]);
                let shown = display_segments(&decode_field(&line[i..=i], enc))
                    .iter()
                    .map(|s| s.text.to_string())
                    .collect::<String>();
                pieces.push(PreviewPiece {
                    text: shown,
                    delimiter: true,
                    escaped: false,
                });
                start = i + 1;
            }
            i += 1;
        }
        push_text(&mut pieces, &line[start.min(line.len())..]);
        lines.push(pieces);
        pos = end + 1;
    }
    lines
}

pub fn draw(frame: &mut Frame, area: Rect, d: &DetectedFormatState, state: &AppState) {
    let theme = &state.theme;
    let width = MAX_WIDTH.min(area.width.saturating_sub(4));
    let rect = centered_rect(width, HEIGHT, area);
    let inner = dialog_frame(frame, rect, "Detected format", theme.teal, theme);
    if inner.height < 13 || inner.width < 10 {
        return;
    }
    let tab = state.tabs.iter().find(|t| t.id == d.tab);
    let loaded = tab.and_then(|t| t.loaded.as_ref());
    let buf = frame.buffer_mut();
    let base = theme.dialog();
    let x = inner.x + 1;
    let w = inner.width - 2;
    let label = base.patch(theme.dim());
    let value = base;
    let changed = base.patch(theme.value_changed());

    let cur = &d.current;
    let det = &d.detected;
    let quoted_newlines = loaded.is_some_and(|l| l.sniff.quoted_newlines);
    let rows: [(&str, String, bool); 7] = [
        (
            "delimiter",
            delimiter_name(cur.delimiter),
            cur.delimiter != det.delimiter,
        ),
        ("quote", quote_name(cur.quote), cur.quote != det.quote),
        (
            "escape",
            match cur.escape {
                EscapeStyle::Doubled => "doubled",
                EscapeStyle::Backslash => "backslash",
            }
            .to_owned(),
            cur.escape != det.escape,
        ),
        (
            "header",
            if cur.header { "yes" } else { "no" }.to_owned(),
            cur.header != det.header,
        ),
        (
            "encoding",
            match loaded.and_then(|l| l.original_encoding) {
                Some(enc) => format!("{} → utf-8", enc.name()),
                None => cur.encoding.name().to_owned(),
            },
            false,
        ),
        (
            "line end",
            match cur.line_ending {
                LineEnding::Lf => "LF",
                LineEnding::CrLf => "CRLF",
                LineEnding::Mixed => "mixed",
            }
            .to_owned(),
            false,
        ),
        (
            "quoted newlines",
            if quoted_newlines { "yes" } else { "no" }.to_owned(),
            false,
        ),
    ];
    for (i, (name, text, is_changed)) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        let after = put(buf, x, y, &format!("{name:<LABEL_WIDTH$}"), label);
        if *is_changed {
            put(buf, after, y, &format!("{text}*"), changed);
        } else {
            put(buf, after, y, text, value);
        }
    }

    // Preview: the first 3 raw lines with the delimiters highlighted.
    if let Some(l) = loaded {
        let styles = PreviewStyles {
            plain: base,
            escaped: base.patch(theme.control_char()),
            delimiter: theme.cursor_cell(),
            ellipsis: base.patch(theme.dim()),
        };
        for (i, line) in preview(&l.source, cur).iter().enumerate() {
            draw_preview_line(buf, x, inner.y + 8 + i as u16, w, line, &styles);
        }
    }
    let footer = text::truncate_end(FOOTER, usize::from(w));
    put(buf, x, inner.y + 12, &footer, base.patch(theme.hint()));
}

struct PreviewStyles {
    plain: Style,
    escaped: Style,
    delimiter: Style,
    ellipsis: Style,
}

/// Draws one preview line, cut with `…` at `width` cells.
fn draw_preview_line(
    buf: &mut ratatui::buffer::Buffer,
    x0: u16,
    y: u16,
    width: u16,
    pieces: &[PreviewPiece],
    styles: &PreviewStyles,
) {
    let total: usize = pieces.iter().map(|p| text::width(&p.text)).sum();
    let fits = total <= usize::from(width);
    let limit = if fits {
        usize::from(width)
    } else {
        usize::from(width.saturating_sub(1))
    };
    let mut used = 0;
    let mut x = x0;
    for p in pieces {
        let style = if p.delimiter {
            styles.delimiter
        } else if p.escaped {
            styles.escaped
        } else {
            styles.plain
        };
        let w = text::width(&p.text);
        if used + w <= limit {
            x = put(buf, x, y, &p.text, style);
            used += w;
        } else {
            let (end, _) = text::prefix_within(&p.text, limit - used);
            x = put(buf, x, y, &p.text[..end], style);
            break;
        }
    }
    if !fits {
        put(buf, x, y, text::ELLIPSIS, styles.ellipsis);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn delimiter_cycle() {
        let mut d = b',';
        let mut seen = vec![d];
        for _ in 0..6 {
            d = cycle_delimiter(d, b',');
            seen.push(d);
        }
        assert_eq!(seen, b",\t|;: ,".to_vec());
        // A custom detected delimiter appears once, first.
        let mut d = b'#';
        let mut seen = vec![d];
        for _ in 0..7 {
            d = cycle_delimiter(d, b'#');
            seen.push(d);
        }
        assert_eq!(seen, b"#,\t|;: #".to_vec());
        // A detected tab starts the list at its own place.
        assert_eq!(cycle_delimiter(b'\t', b'\t'), b'|');
    }

    #[test]
    fn quote_cycle() {
        assert_eq!(cycle_quote(Some(b'"')), Some(b'\''));
        assert_eq!(cycle_quote(Some(b'\'')), None);
        assert_eq!(cycle_quote(None), Some(b'"'));
    }

    #[test]
    fn delimiter_names() {
        assert_eq!(delimiter_name(b','), ", (comma)");
        assert_eq!(delimiter_name(b'\t'), "\\t (tab)");
        assert_eq!(delimiter_name(b'#'), "#");
        assert_eq!(delimiter_name(0x1f), "0x1f");
    }

    #[test]
    fn debounce() {
        let t0 = Instant::now();
        let d = Dialect::default();
        let mut s = DetectedFormatState::new(TabId(1), d, d);
        s.cycle_delimiter(t0);
        s.cycle_delimiter(t0 + Duration::from_millis(100));
        assert_eq!(s.due(t0 + Duration::from_millis(200)), None);
        let due = s.due(t0 + Duration::from_millis(250)).unwrap();
        assert_eq!(due.delimiter, b'|');
        assert_eq!(s.due(t0 + Duration::from_secs(9)), None);
    }

    #[test]
    fn preview_highlights_delimiters_outside_quotes() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"a,\"b,c\"\r\n1,\"x\ny,z\"\n").unwrap();
        let src = Source::open(f.path(), None).unwrap();
        let d = Dialect::default();
        let lines = preview(&src, &d);
        let shown: Vec<String> = lines
            .iter()
            .map(|l| {
                l.iter()
                    .map(|p| {
                        if p.delimiter {
                            format!("[{}]", p.text)
                        } else {
                            p.text.clone()
                        }
                    })
                    .collect()
            })
            .collect();
        // The quote state carries over the quoted newline.
        assert_eq!(shown, ["a[,]\"b,c\"", "1[,]\"x", "y,z\""]);
    }
}
