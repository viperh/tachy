//! The `g` go-to dialog (spec §13, §11.7, M2-03, D8): a 50×7 teal dialog
//! with one input line and one hint or error line. Parsing is in
//! [`crate::goto`]; `App` executes the target.

use ratatui::{Frame, layout::Rect};
use tachy_core::text;

use super::{dialog_frame, draw_input};
use crate::{components::layout::centered_rect, components::put, input::LineInput, theme::Theme};

/// Width and height of the dialog.
pub const SIZE: (u16, u16) = (50, 7);
const HINT: &str = "row number · 50% · column name";
const FOOTER: &str = "Enter go · Esc cancel";
const PROMPT: &str = "› ";

/// The dialog's input and its error, reset each time it opens.
#[derive(Debug, Clone, Default)]
pub struct GotoDialog {
    pub input: LineInput,
    /// Shown in coral instead of the hint, until the input changes.
    pub error: Option<String>,
}

pub fn draw(frame: &mut Frame, area: Rect, dialog: &GotoDialog, theme: &Theme) {
    let rect = centered_rect(SIZE.0, SIZE.1, area);
    let inner = dialog_frame(frame, rect, "Go to", theme.teal, theme);
    if inner.height < 5 || inner.width < 4 {
        return;
    }
    let buf = frame.buffer_mut();
    let base = theme.dialog();
    let x = inner.x + 1;
    let width = inner.width - 2;
    let y = inner.y + 1;
    let after = put(buf, x, y, PROMPT, base.patch(theme.query_prompt()));
    draw_input(
        buf,
        after,
        y,
        width.saturating_sub(after - x),
        &dialog.input,
        base,
        theme,
    );
    let (line, style) = match &dialog.error {
        Some(err) => (err.as_str(), theme.inline_error()),
        None => (HINT, theme.dim()),
    };
    let line = text::truncate_end(line, usize::from(width));
    put(buf, x, y + 1, &line, base.patch(style));
    put(buf, x, inner.y + 4, FOOTER, base.patch(theme.hint()));
}
