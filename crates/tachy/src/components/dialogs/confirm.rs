//! The generic `y`/`n` confirm dialog (M5-01): `q` or `Ctrl-w` with jobs
//! running (`2 jobs running — quit anyway? (y/n)`), `K` on a sort past
//! 50 % (`kill sort? (y/n)`). `y` / `Enter` confirm (`Submit`), `n` / `Esc`
//! cancel (`KeyContext::Confirm`).

use ratatui::{Frame, layout::Rect};
use tachy_core::text;

use super::dialog_frame;
use crate::{
    components::{layout::centered_rect, put},
    jobs::JobId,
    tab::TabId,
    theme::Theme,
};

/// Smallest and largest width of the dialog.
const MIN_WIDTH: u16 = 40;
const MAX_WIDTH: u16 = 72;
const HEIGHT: u16 = 6;
const FOOTER: &str = "y yes · n no";

/// What `y` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Cancel every job and quit.
    Quit,
    /// Close this tab (or quit, when it is the last one).
    CloseTab(TabId),
    /// Kill this job.
    KillJob(JobId),
}

/// The open confirm dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmState {
    /// The question, with its `(y/n)`.
    pub text: String,
    pub action: ConfirmAction,
}

impl ConfirmState {
    /// `N jobs running — <what> anyway? (y/n)`.
    pub fn jobs_running(n: usize, what: &str, action: ConfirmAction) -> Self {
        let noun = if n == 1 { "job" } else { "jobs" };
        ConfirmState {
            text: format!("{n} {noun} running — {what} anyway? (y/n)"),
            action,
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, c: &ConfirmState, theme: &Theme) {
    let width = (text::width(&c.text) as u16 + 6).clamp(MIN_WIDTH, MAX_WIDTH);
    let rect = centered_rect(width, HEIGHT, area);
    let inner = dialog_frame(frame, rect, "Confirm", theme.amber, theme);
    if inner.height < 4 || inner.width < 4 {
        return;
    }
    let buf = frame.buffer_mut();
    let base = theme.dialog();
    let w = usize::from(inner.width - 2);
    let x = inner.x + 1;
    put(buf, x, inner.y + 1, &text::truncate_end(&c.text, w), base);
    put(buf, x, inner.y + 3, FOOTER, base.patch(theme.hint()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts() {
        assert_eq!(
            ConfirmState::jobs_running(1, "quit", ConfirmAction::Quit).text,
            "1 job running — quit anyway? (y/n)"
        );
        assert_eq!(
            ConfirmState::jobs_running(3, "close tab", ConfirmAction::Quit).text,
            "3 jobs running — close tab anyway? (y/n)"
        );
    }
}
