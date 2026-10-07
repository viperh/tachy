//! Top bar: app name, tabs, view kind, mode pill (spec §11.2, M1-07).
//!
//! ` tachy ` · ` 1 a.csv ` ` 2 b.csv ` … · spacer · `view: all rows` ·
//! ` NORMAL `. When the tabs don't fit, the active tab stays visible, tabs
//! are dropped from the far end and `…` marks where.

use std::ops::Range;

use ratatui::{Frame, layout::Rect};
use tachy_core::text;

use super::{Component, put};
use crate::state::AppState;

/// File names longer than this are cut in the middle (§11.2).
pub const MAX_TAB_NAME: usize = 24;
/// Width of the `…` drawn where tabs were dropped.
const MARKER_WIDTH: u16 = 1;

/// Draws the top bar.
#[derive(Debug, Default)]
pub struct TopBar;

/// The label of tab `i` (0-based): ` N name `; tabs past the 9th get no
/// number (`1`–`9` switch tabs).
pub fn tab_label(i: usize, name: &str) -> String {
    let name = text::truncate_middle(name, MAX_TAB_NAME);
    if i < 9 {
        format!(" {} {name} ", i + 1)
    } else {
        format!(" {name} ")
    }
}

/// The range of tabs to draw in `avail` cells, keeping `active` visible.
/// Starts from the first tab when the active one fits that way; otherwise
/// the window ends at the active tab and grows to the left, then the right.
/// `…` markers (1 cell each) are counted where tabs are dropped.
pub fn tab_window(widths: &[u16], active: usize, avail: u16) -> Range<usize> {
    let n = widths.len();
    let fits = |lo: usize, hi: usize| {
        let tabs: u32 = widths[lo..hi].iter().map(|&w| u32::from(w)).sum();
        let markers = u32::from(MARKER_WIDTH) * (u32::from(lo > 0) + u32::from(hi < n));
        tabs + markers <= u32::from(avail)
    };
    let mut hi = 0;
    while hi < n && fits(0, hi + 1) {
        hi += 1;
    }
    if active < hi || n == 0 {
        return 0..hi;
    }
    let active = active.min(n - 1);
    let (mut lo, mut hi) = (active, active + 1);
    while lo > 0 && fits(lo - 1, hi) {
        lo -= 1;
    }
    while hi < n && fits(lo, hi + 1) {
        hi += 1;
    }
    lo..hi
}

impl Component for TopBar {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let theme = &state.theme;
        let buf = frame.buffer_mut();
        let surface = theme.surface();
        buf.set_style(area, surface);
        let y = area.y;
        let mut x = put(buf, area.x, y, " tachy ", surface.patch(theme.app_name()));

        // Right side: view kind and the mode pill.
        let mode = state.visible_mode();
        let pill = format!(" {} ", mode.label());
        let view = state
            .active_tab()
            .map(|t| format!("view: {}  ", t.view_label()))
            .unwrap_or_default();
        let right_w = (text::width(&view) + text::width(&pill)) as u16;
        let right_x = area.right().saturating_sub(right_w);
        put(buf, right_x, y, &view, surface.patch(theme.dim()));
        put(
            buf,
            right_x + text::width(&view) as u16,
            y,
            &pill,
            theme.mode_pill(mode),
        );

        // Tabs in between, with one cell of gap before the right side.
        let labels: Vec<String> = state
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| tab_label(i, &t.name))
            .collect();
        let widths: Vec<u16> = labels.iter().map(|l| text::width(l) as u16).collect();
        let avail = right_x.saturating_sub(x).saturating_sub(1);
        let window = tab_window(&widths, state.active_tab, avail);
        let limit = x + avail;
        if window.start > 0 {
            x = put(buf, x, y, "…", surface.patch(theme.tab()));
        }
        for i in window.clone() {
            let style = if i == state.active_tab {
                theme.tab_active()
            } else {
                surface.patch(theme.tab())
            };
            let label = text::truncate_end(&labels[i], usize::from(limit.saturating_sub(x)));
            x = put(buf, x, y, &label, style);
        }
        if window.end < labels.len() && x < limit + 1 {
            put(buf, x, y, "…", surface.patch(theme.tab()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(tab_label(0, "a.csv"), " 1 a.csv ");
        assert_eq!(tab_label(8, "a.csv"), " 9 a.csv ");
        assert_eq!(tab_label(9, "a.csv"), " a.csv ");
        assert_eq!(
            tab_label(0, "orders_2024_full_export_03.csv"),
            " 1 orders_2024_…port_03.csv "
        );
    }

    #[test]
    fn all_tabs_fit() {
        assert_eq!(tab_window(&[5, 5, 5], 2, 15), 0..3);
        assert_eq!(tab_window(&[], 0, 15), 0..0);
    }

    #[test]
    fn far_end_is_dropped_when_the_active_tab_fits() {
        // 5+5+1 (marker) = 11.
        assert_eq!(tab_window(&[5, 5, 5, 5], 1, 12), 0..2);
    }

    #[test]
    fn active_tab_stays_visible() {
        let widths = [10; 12];
        let w = tab_window(&widths, 10, 45);
        assert!(w.contains(&10), "{w:?}");
        // 1 + 4×10 = 41 ≤ 45 with markers on both sides: 1 + 40 + 1.
        assert_eq!(w.len(), 4);
        assert_eq!(tab_window(&widths, 11, 45), 8..12);
    }
}
