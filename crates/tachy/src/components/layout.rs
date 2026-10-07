//! Screen regions (spec §11.1) and overlay geometry (§11.7).
//!
//! [`compute_layout`] is a pure function of the terminal area and a few
//! booleans, so every rule here is unit-tested without a terminal.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Widget},
};

use crate::theme::Theme;

/// Minimum terminal size (§11.1). Below it only a message is drawn.
pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;
/// The inspector is shown only from this terminal width on (§11.1).
pub const INSPECTOR_MIN_TERMINAL_WIDTH: u16 = 120;
/// Width of the inspector panel, without its divider (§11.1).
pub const INSPECTOR_WIDTH: u16 = 40;
/// The jobs drawer lists at most this many jobs (§11.1).
pub const JOBS_DRAWER_MAX_JOBS: u16 = 7;

/// What [`compute_layout`] needs to know about the app state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutInput {
    /// Show the 2-line query bar: filter or search mode, an open `Ctrl-o`
    /// prompt (D8), or a running filter job on the active view (M4-04).
    pub query_bar: bool,
    /// `Some(n)` when the jobs drawer is open and lists `n` jobs. The drawer
    /// is `1 + min(n, 7)` rows high.
    pub jobs_drawer_rows: Option<u16>,
    /// A toast is visible.
    pub toast: bool,
    /// The key-hint line is visible.
    pub hints: bool,
    /// The inspector is toggled on (it still needs 120 columns).
    pub inspector: bool,
}

/// Every region of the screen, top to bottom (§11.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppLayout {
    /// Always 1 row.
    pub top_bar: Rect,
    /// 2 rows, when [`LayoutInput::query_bar`].
    pub query_bar: Option<Rect>,
    /// Column names and types: always 2 rows. Narrowed by the inspector.
    pub table_header: Rect,
    /// Fills the remaining rows. Narrowed by the inspector.
    pub table_body: Rect,
    /// Right of the table region (header + body rows), exactly 40 columns.
    /// Only when toggled on and the terminal is at least 120 columns wide.
    pub inspector: Option<Rect>,
    /// The 1-column divider left of the inspector.
    pub inspector_divider: Option<Rect>,
    /// `1 + min(jobs, 7)` rows, when the drawer is open.
    pub jobs_drawer: Option<Rect>,
    /// One full-width row that **overlays** the last row of `table_body`, or
    /// the last row of `jobs_drawer` when the drawer is open (§16: "one line,
    /// above the status line"). A toast never takes a row of its own, so the
    /// table doesn't jump when toasts come and go.
    pub toast: Option<Rect>,
    /// Always 1 row.
    pub status: Rect,
    /// 1 row, when [`LayoutInput::hints`].
    pub hints: Option<Rect>,
    /// The terminal is smaller than 80×24: draw only the "terminal too
    /// small" message ([`render_too_small`]).
    pub too_small: bool,
}

impl AppLayout {
    /// The table component's area: header and body together.
    pub fn table(&self) -> Rect {
        self.table_header.union(self.table_body)
    }
}

/// Splits `area` into the §11.1 regions.
pub fn compute_layout(area: Rect, s: &LayoutInput) -> AppLayout {
    let too_small = area.width < MIN_WIDTH || area.height < MIN_HEIGHT;

    let query_h = if s.query_bar { 2 } else { 0 };
    let drawer_h = s
        .jobs_drawer_rows
        .map(|jobs| 1 + jobs.min(JOBS_DRAWER_MAX_JOBS));
    let hints_h = u16::from(s.hints);
    let fixed = 1 + query_h + 2 + drawer_h.unwrap_or(0) + 1 + hints_h;
    let body_h = area.height.saturating_sub(fixed);

    // Hands out rows top to bottom, never past the bottom of `area`.
    let mut y = area.y;
    let bottom = area.bottom();
    let mut take = |h: u16| {
        let h = h.min(bottom.saturating_sub(y));
        let rect = Rect::new(area.x, y, area.width, h);
        y += h;
        rect
    };

    let top_bar = take(1);
    let query_bar = s.query_bar.then(|| take(query_h));
    let mut table_header = take(2);
    let mut table_body = take(body_h);
    let jobs_drawer = drawer_h.map(&mut take);
    let status = take(1);
    let hints = s.hints.then(|| take(hints_h));

    let (inspector, inspector_divider) =
        if s.inspector && area.width >= INSPECTOR_MIN_TERMINAL_WIDTH {
            let y = table_header.y;
            let h = table_header.height + table_body.height;
            let x = area.right() - INSPECTOR_WIDTH;
            table_header.width -= INSPECTOR_WIDTH + 1;
            table_body.width -= INSPECTOR_WIDTH + 1;
            (
                Some(Rect::new(x, y, INSPECTOR_WIDTH, h)),
                Some(Rect::new(x - 1, y, 1, h)),
            )
        } else {
            (None, None)
        };

    let toast = if s.toast {
        let host = jobs_drawer.unwrap_or(table_body);
        (host.height > 0).then(|| Rect::new(area.x, host.bottom() - 1, area.width, 1))
    } else {
        None
    };

    AppLayout {
        top_bar,
        query_bar,
        table_header,
        table_body,
        inspector,
        inspector_divider,
        jobs_drawer,
        toast,
        status,
        hints,
        too_small,
    }
}

/// A `width`×`height` rect centred in `area`, clamped to it (§11.7 dialogs).
pub fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// The command palette (§11.7): 96 columns wide (or the terminal width − 8
/// if narrower), centred horizontally, top-anchored at row 3, at most 20 rows.
pub fn palette_rect(area: Rect) -> Rect {
    let width = 96.min(area.width.saturating_sub(8));
    let height = area.height.saturating_sub(4).min(20);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + 3.min(area.height),
        width,
        height,
    )
}

/// Dims everything already drawn in `area` behind a modal (§11.7, §14):
/// restyles each cell with `theme.backdrop()` without clearing it.
pub fn apply_backdrop(buf: &mut Buffer, area: Rect, theme: &Theme) {
    let area = area.intersection(buf.area);
    let style = theme.backdrop();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(style);
        }
    }
}

/// Fills `area` with a vertical line (`│`) in `style`.
pub fn render_vertical_divider(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_symbol("│").set_style(style);
        }
    }
}

/// The only thing drawn below 80×24 (§11.1): a centred 2-line message on
/// `theme.base()`.
pub fn render_too_small(buf: &mut Buffer, area: Rect, theme: &Theme) {
    buf.set_style(area, theme.base());
    let lines = vec![
        Line::from("terminal too small"),
        Line::from(format!(
            "{}×{} — need {MIN_WIDTH}×{MIN_HEIGHT}",
            area.width, area.height
        )),
    ];
    let rect = centered_rect(area.width, 2, area);
    Paragraph::new(lines)
        .style(theme.base())
        .centered()
        .render(rect, buf);
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const AREA: Rect = Rect::new(0, 0, 120, 40);

    fn input() -> LayoutInput {
        LayoutInput {
            hints: true,
            inspector: true,
            ..Default::default()
        }
    }

    #[test]
    fn always_on_regions_at_120x40() {
        let l = compute_layout(AREA, &input());
        assert!(!l.too_small);
        assert_eq!(l.top_bar, Rect::new(0, 0, 120, 1));
        assert_eq!(l.query_bar, None);
        assert_eq!(l.table_header, Rect::new(0, 1, 79, 2));
        assert_eq!(l.table_body, Rect::new(0, 3, 79, 35));
        assert_eq!(l.inspector, Some(Rect::new(80, 1, 40, 37)));
        assert_eq!(l.inspector_divider, Some(Rect::new(79, 1, 1, 37)));
        assert_eq!(l.jobs_drawer, None);
        assert_eq!(l.toast, None);
        assert_eq!(l.status, Rect::new(0, 38, 120, 1));
        assert_eq!(l.hints, Some(Rect::new(0, 39, 120, 1)));
        assert_eq!(l.table(), Rect::new(0, 1, 79, 37));
    }

    #[test]
    fn query_bar_takes_two_rows() {
        let l = compute_layout(
            AREA,
            &LayoutInput {
                query_bar: true,
                ..input()
            },
        );
        assert_eq!(l.query_bar, Some(Rect::new(0, 1, 120, 2)));
        assert_eq!(l.table_header.y, 3);
        assert_eq!(l.table_body.height, 33);
    }

    #[test]
    fn inspector_needs_120_columns() {
        let narrow = compute_layout(Rect::new(0, 0, 119, 40), &input());
        assert_eq!(narrow.inspector, None);
        assert_eq!(narrow.inspector_divider, None);
        assert_eq!(narrow.table_body.width, 119);

        let wide = compute_layout(AREA, &input());
        assert_eq!(wide.inspector.unwrap().width, 40);

        let off = compute_layout(
            AREA,
            &LayoutInput {
                inspector: false,
                ..input()
            },
        );
        assert_eq!(off.inspector, None);
        assert_eq!(off.table_body.width, 120);
    }

    #[test]
    fn minimum_size() {
        assert!(compute_layout(Rect::new(0, 0, 79, 24), &input()).too_small);
        assert!(compute_layout(Rect::new(0, 0, 80, 23), &input()).too_small);
        assert!(!compute_layout(Rect::new(0, 0, 80, 24), &input()).too_small);
    }

    #[test]
    fn jobs_drawer_height() {
        for (jobs, height) in [(0, 1), (3, 4), (7, 8), (10, 8)] {
            let l = compute_layout(
                AREA,
                &LayoutInput {
                    jobs_drawer_rows: Some(jobs),
                    ..input()
                },
            );
            let drawer = l.jobs_drawer.unwrap();
            assert_eq!(drawer.height, height, "{jobs} jobs");
            assert_eq!(drawer.bottom(), l.status.y, "{jobs} jobs");
            assert_eq!(l.table_body.height, 35 - height, "{jobs} jobs");
        }
    }

    #[test]
    fn hiding_hints_grows_the_body() {
        let with = compute_layout(AREA, &input());
        let without = compute_layout(
            AREA,
            &LayoutInput {
                hints: false,
                ..input()
            },
        );
        assert_eq!(without.hints, None);
        assert_eq!(without.table_body.height, with.table_body.height + 1);
        assert_eq!(without.status.bottom(), 40);
    }

    #[test]
    fn toast_overlays_the_last_body_row() {
        let without = compute_layout(AREA, &input());
        let with = compute_layout(
            AREA,
            &LayoutInput {
                toast: true,
                ..input()
            },
        );
        assert_eq!(with.table_body, without.table_body);
        assert_eq!(with.toast, Some(Rect::new(0, 37, 120, 1)));
        assert_eq!(with.toast.unwrap().bottom(), with.status.y);
    }

    #[test]
    fn toast_overlays_the_last_drawer_row() {
        let l = compute_layout(
            AREA,
            &LayoutInput {
                toast: true,
                jobs_drawer_rows: Some(3),
                ..input()
            },
        );
        let drawer = l.jobs_drawer.unwrap();
        assert_eq!(l.toast, Some(Rect::new(0, drawer.bottom() - 1, 120, 1)));
        assert_eq!(drawer.height, 4);
    }

    #[test]
    fn centered_rect_is_clamped() {
        assert_eq!(centered_rect(20, 10, AREA), Rect::new(50, 15, 20, 10));
        assert_eq!(
            centered_rect(200, 100, Rect::new(5, 5, 10, 4)),
            Rect::new(5, 5, 10, 4)
        );
    }

    #[test]
    fn palette_geometry() {
        assert_eq!(palette_rect(AREA), Rect::new(12, 3, 96, 20));
        assert_eq!(
            palette_rect(Rect::new(0, 0, 80, 24)),
            Rect::new(4, 3, 72, 20)
        );
    }

    #[test]
    fn backdrop_keeps_symbols_and_changes_styles() {
        let theme = Theme::DARK;
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "ab", theme.cursor_cell());
        buf.set_string(0, 1, "cd", theme.base());
        let before = buf.clone();

        apply_backdrop(&mut buf, area, &theme);

        for y in 0..2 {
            for x in 0..4 {
                let (old, new) = (&before[(x, y)], &buf[(x, y)]);
                assert_eq!(old.symbol(), new.symbol());
                assert_eq!(new.fg, theme.fg_dim);
                assert!(new.modifier.contains(ratatui::style::Modifier::DIM));
            }
        }
        assert_ne!(before, buf);
    }
}
