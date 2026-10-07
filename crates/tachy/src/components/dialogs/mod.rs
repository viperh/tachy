//! Modal dialogs (spec §11.7): Go to (M2-03), Detected format (M2-04), the
//! full-value popup (M3-03), the column chooser (M3-04), the `y`/`n`
//! confirm dialog (M5-01) and Export (M6-01).
//!
//! Each dialog's state is plain data in [`AppState`] (built and changed by
//! `App`, which owns the tabs the dialogs act on); this component only
//! draws the open one. Every dialog is centred, on `surface_raised`, with a
//! 1-cell border and a title bar in its accent colour, over the dimmed
//! backdrop that `App` applies first.

pub mod column_chooser;
pub mod confirm;
pub mod detected_format;
pub mod export;
pub mod goto;
pub mod value_popup;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Clear},
};
use tachy_core::text;

use super::{Component, put};
use crate::{
    input::LineInput,
    state::{AppState, DialogKind, Overlay},
    theme::Theme,
};

/// Draws the open dialog (`AppState::overlay == Some(Overlay::Dialog(_))`),
/// centred in `area` (the whole screen).
#[derive(Debug, Default)]
pub struct Dialogs;

impl Component for Dialogs {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let Some(Overlay::Dialog(kind)) = state.overlay else {
            return Ok(());
        };
        let theme = &state.theme;
        match kind {
            DialogKind::Goto => goto::draw(frame, area, &state.goto, theme),
            DialogKind::DetectedFormat => {
                if let Some(d) = &state.detected {
                    detected_format::draw(frame, area, d, state);
                }
            }
            DialogKind::ValuePopup => {
                if let Some(p) = &state.value_popup {
                    value_popup::draw(frame, area, p, theme);
                }
            }
            DialogKind::ColumnChooser => {
                if let Some(c) = &state.chooser {
                    column_chooser::draw(frame, area, c, state);
                }
            }
            DialogKind::Confirm => {
                if let Some(c) = &state.confirm {
                    confirm::draw(frame, area, c, theme);
                }
            }
            DialogKind::Export => {
                if let Some(f) = &state.export {
                    export::draw(frame, area, f, theme);
                }
            }
        }
        Ok(())
    }
}

/// Clears `rect`, draws the border and the title bar in `accent`, and
/// returns the inner area.
pub fn dialog_frame(
    frame: &mut Frame,
    rect: Rect,
    title: &str,
    accent: Color,
    theme: &Theme,
) -> Rect {
    let block = Block::bordered()
        .style(theme.dialog())
        .border_style(theme.dialog_border(accent))
        .title(format!(" {title} "))
        .title_style(theme.dialog_title(accent));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    inner
}

/// A one-line text input with a block cursor at `(x, y)`, `width` cells
/// wide. A long input scrolls so the cursor stays visible.
pub fn draw_input(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    input: &LineInput,
    base: Style,
    theme: &Theme,
) {
    let avail = usize::from(width);
    let (before, after) = input.text().split_at(input.cursor());
    let mut shown_before = before.to_owned();
    while text::width(&shown_before) + 1 > avail && !shown_before.is_empty() {
        shown_before.remove(0);
    }
    let right = x + width;
    let x = put(buf, x, y, &shown_before, base);
    let mut chars = after.chars();
    let under = chars.next().map_or(" ".to_owned(), |c| c.to_string());
    let x = put(buf, x, y, &under, theme.cursor_cell());
    let rest = text::truncate_end(chars.as_str(), usize::from(right.saturating_sub(x)));
    put(buf, x, y, &rest, base);
}
