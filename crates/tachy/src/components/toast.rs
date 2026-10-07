//! The toast line, drawn over the last body (or jobs drawer) row (spec §16,
//! M1-09).
//!
//! ` text… (+N) `: one space of padding on each side, the text cut with `…`
//! when it doesn't fit, and the number of waiting toasts in dim at the right.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::Component;
use crate::state::AppState;

/// Draws the front toast of `AppState::toasts`, if any.
#[derive(Debug, Default)]
pub struct Toast;

impl Component for Toast {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let Some(toast) = state.toasts.front() else {
            return Ok(());
        };
        let theme = &state.theme;
        let style = theme.toast(toast.level);
        let width = usize::from(area.width);

        let waiting = state.toasts.waiting();
        let count = (waiting > 0).then(|| format!("(+{waiting})"));
        // Padding on both sides, plus a space before the count.
        let reserved = 2 + count.as_ref().map_or(0, |c| c.width() + 1);
        let text = truncate(&toast.text, width.saturating_sub(reserved));
        let gap = width.saturating_sub(reserved + text.width());

        let mut spans = vec![Span::raw(" "), Span::raw(text), Span::raw(" ".repeat(gap))];
        if let Some(count) = count {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(count, style.patch(theme.dim())));
        }
        spans.push(Span::raw(" "));
        // Reset the row first, so modifiers of what's below (DIM, BOLD) don't
        // leak into the toast; matters most under `NO_COLOR`.
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(Line::from(spans)).style(style), area);
        Ok(())
    }
}

/// `text` cut to at most `max` display columns, ending in `…` when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let Some(budget) = max.checked_sub(1) else {
        return String::new();
    };
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_cuts_with_an_ellipsis() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
        // Wide characters never overflow.
        assert_eq!(truncate("日本語", 4), "日…");
        assert_eq!(truncate("日本語", 5), "日本…");
    }
}
