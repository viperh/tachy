//! The column chooser `c` (spec §13, M3-04): show, hide and reorder columns.
//!
//! A purple dialog (a command-like action), 60 wide and `min(h − 4,
//! columns + 4)` high, listing **all** columns (`_extraN` included) in the
//! current display order as `[x] name  type`, frozen ones marked `❄`. Edits
//! happen on a copy; `Enter` applies it to the tab, `Esc` discards it.
//!
//! | Key | Action | Effect |
//! |---|---|---|
//! | `j` `k` `↓` `↑` | `SelectNext` / `SelectPrev` | move the selection |
//! | `Space` | `ToggleVisible` | show / hide the selected column |
//! | `J` `K` `shift-↓` `shift-↑` | `MoveColumnDown` / `MoveColumnUp` | move it in the order |
//! | `a` | `ShowAll` | show every column |
//! | `/` | `FilterList` | type to filter the list by name; `Enter` keeps the filter, `Esc` clears it |
//! | `Enter` | `Submit` | apply and close |
//! | `Esc` | `Cancel` | discard and close |

use ratatui::{Frame, layout::Rect};
use tachy_core::{column::ColumnMeta, text};

use super::{dialog_frame, draw_input};
use crate::{
    components::{layout::centered_rect, put},
    input::LineInput,
    state::AppState,
    tab::{ColumnLayout, TabId},
};

/// Width of the dialog.
pub const WIDTH: u16 = 60;
const FOOTER: &str = "␣ toggle  J/K move  a all  / filter  ⏎ apply  Esc cancel";
pub const LAST_VISIBLE: &str = "at least one column must be visible";

/// The chooser's working copy. `App` keeps it in `AppState::chooser`.
#[derive(Debug, Clone, Default)]
pub struct ColumnChooserState {
    pub tab: TabId,
    /// Source columns in display order (all of them).
    pub order: Vec<usize>,
    /// Indexed by source column.
    pub visible: Vec<bool>,
    /// The requested `freeze`, for the `❄` markers.
    pub freeze: usize,
    /// Index into [`ColumnChooserState::rows`].
    pub selected: usize,
    pub filter: LineInput,
    /// Keys go to the filter input (after `/`).
    pub filtering: bool,
    pub error: Option<String>,
}

impl ColumnChooserState {
    pub fn new(tab: TabId, layout: &ColumnLayout, cursor: Option<usize>) -> Self {
        let selected = cursor
            .and_then(|c| layout.order.iter().position(|&o| o == c))
            .unwrap_or(0);
        ColumnChooserState {
            tab,
            order: layout.order.clone(),
            visible: layout.visible.clone(),
            freeze: layout.freeze,
            selected,
            ..ColumnChooserState::default()
        }
    }

    /// Positions in `order` of the listed rows: those matching the filter
    /// (case-insensitive substring of the display or query name).
    pub fn rows(&self, columns: &[ColumnMeta]) -> Vec<usize> {
        let needle = self.filter.text().to_lowercase();
        (0..self.order.len())
            .filter(|&i| {
                needle.is_empty()
                    || columns.get(self.order[i]).is_some_and(|c| {
                        c.name.display.to_lowercase().contains(&needle)
                            || c.name.query.to_lowercase().contains(&needle)
                    })
            })
            .collect()
    }

    /// The position in `order` of the selected row.
    fn selected_pos(&self, columns: &[ColumnMeta]) -> Option<usize> {
        self.rows(columns).get(self.selected).copied()
    }

    /// Keeps the selection inside the list (after the filter changed).
    pub fn clamp(&mut self, columns: &[ColumnMeta]) {
        let n = self.rows(columns).len();
        self.selected = self.selected.min(n.saturating_sub(1));
    }

    pub fn select(&mut self, delta: isize, columns: &[ColumnMeta]) {
        let n = self.rows(columns).len();
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(n.saturating_sub(1));
    }

    /// `Space`. Hiding the last visible column is refused with an error.
    pub fn toggle_visible(&mut self, columns: &[ColumnMeta]) {
        let Some(pos) = self.selected_pos(columns) else {
            return;
        };
        let col = self.order[pos];
        if self.visible[col] && self.visible.iter().filter(|v| **v).count() == 1 {
            self.error = Some(LAST_VISIBLE.to_owned());
            return;
        }
        self.visible[col] = !self.visible[col];
        self.error = None;
    }

    /// `J` / `K`: swaps the selected column with its neighbour in the full
    /// order; the selection follows it.
    pub fn move_selected(&mut self, down: bool, columns: &[ColumnMeta]) {
        let Some(pos) = self.selected_pos(columns) else {
            return;
        };
        let other = if down {
            pos + 1
        } else {
            match pos.checked_sub(1) {
                Some(p) => p,
                None => return,
            }
        };
        if other >= self.order.len() {
            return;
        }
        self.order.swap(pos, other);
        if let Some(i) = self.rows(columns).iter().position(|&r| r == other) {
            self.selected = i;
        }
        self.error = None;
    }

    /// `a`.
    pub fn show_all(&mut self) {
        self.visible.iter_mut().for_each(|v| *v = true);
        self.error = None;
    }

    /// Source columns frozen with the edited order: the first `freeze`
    /// visible ones.
    fn frozen(&self) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|&c| self.visible.get(c).copied().unwrap_or(false))
            .take(self.freeze)
            .collect()
    }
}

pub fn draw(frame: &mut Frame, area: Rect, c: &ColumnChooserState, state: &AppState) {
    let theme = &state.theme;
    let Some(columns) = state
        .tabs
        .iter()
        .find(|t| t.id == c.tab)
        .and_then(|t| t.loaded.as_ref())
        .map(|l| &l.columns)
    else {
        return;
    };
    let wanted = u16::try_from(c.order.len() + 4).unwrap_or(u16::MAX);
    let height = wanted.min(area.height.saturating_sub(4));
    let rect = centered_rect(WIDTH, height, area);
    let inner = dialog_frame(frame, rect, "Columns", theme.purple, theme);
    if inner.height < 3 || inner.width < 20 {
        return;
    }
    let buf = frame.buffer_mut();
    let base = theme.dialog();
    let x = inner.x + 1;
    let w = inner.width - 2;
    let list_h = usize::from(inner.height - 2);
    let rows = c.rows(columns);
    let frozen = c.frozen();
    // Keep the selection visible.
    let first = c.selected.saturating_sub(list_h.saturating_sub(1));
    for (i, &pos) in rows.iter().enumerate().skip(first).take(list_h) {
        let y = inner.y + (i - first) as u16;
        let col = c.order[pos];
        let Some(meta) = columns.get(col) else {
            continue;
        };
        let selected = i == c.selected;
        let row_style = if selected {
            theme.dialog().patch(theme.row_selected())
        } else {
            base
        };
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), row_style);
        let check = if c.visible.get(col).copied().unwrap_or(false) {
            "[x] "
        } else {
            "[ ] "
        };
        let mut xx = put(buf, x, y, check, row_style);
        let ty = meta.ty().label();
        let ty_w = text::width(ty) as u16;
        let mark = if frozen.contains(&col) { "❄ " } else { "" };
        let name_w = w.saturating_sub(xx - x + ty_w + 2 + text::width(mark) as u16);
        xx = put(buf, xx, y, mark, row_style.patch(theme.dim()));
        let name = text::truncate_end(&meta.name.display, usize::from(name_w));
        put(buf, xx, y, &name, row_style);
        put(
            buf,
            x + w - ty_w,
            y,
            ty,
            row_style.patch(theme.header_type()),
        );
    }
    // The filter or error line, then the footer.
    let y = inner.bottom() - 2;
    if let Some(err) = &c.error {
        let err = text::truncate_end(err, usize::from(w));
        put(buf, x, y, &err, base.patch(theme.inline_error()));
    } else if c.filtering || !c.filter.text().is_empty() {
        let after = put(buf, x, y, "/ ", base.patch(theme.query_prompt()));
        if c.filtering {
            draw_input(
                buf,
                after,
                y,
                w.saturating_sub(after - x),
                &c.filter,
                base,
                theme,
            );
        } else {
            put(buf, after, y, c.filter.text(), base);
        }
        if rows.is_empty() {
            let msg = "no matches";
            put(
                buf,
                x + w - text::width(msg) as u16,
                y,
                msg,
                base.patch(theme.dim()),
            );
        }
    }
    let footer = text::truncate_end(FOOTER, usize::from(w));
    put(
        buf,
        x,
        inner.bottom() - 1,
        &footer,
        base.patch(theme.hint()),
    );
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tachy_core::column::ColumnName;

    use super::*;

    fn cols(names: &[&str]) -> Vec<ColumnMeta> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                ColumnMeta::new(
                    ColumnName {
                        display: (*n).to_owned(),
                        query: (*n).to_owned(),
                    },
                    i,
                    false,
                )
            })
            .collect()
    }

    fn chooser(n: usize) -> ColumnChooserState {
        let layout = ColumnLayout {
            order: (0..n).collect(),
            visible: vec![true; n],
            widths: vec![5; n],
            manual: vec![false; n],
            freeze: 1,
        };
        ColumnChooserState::new(TabId(1), &layout, Some(0))
    }

    #[test]
    fn toggling_and_the_last_visible_column() {
        let c = cols(&["a", "b"]);
        let mut s = chooser(2);
        s.toggle_visible(&c);
        assert_eq!(s.visible, [false, true]);
        s.select(1, &c);
        s.toggle_visible(&c);
        assert_eq!(s.visible, [false, true]);
        assert_eq!(s.error.as_deref(), Some(LAST_VISIBLE));
        s.show_all();
        assert_eq!(s.visible, [true, true]);
        assert_eq!(s.error, None);
    }

    #[test]
    fn moving_columns() {
        let c = cols(&["a", "b", "c"]);
        let mut s = chooser(3);
        s.move_selected(true, &c);
        assert_eq!(s.order, [1, 0, 2]);
        assert_eq!(s.selected, 1);
        s.move_selected(true, &c);
        assert_eq!(s.order, [1, 2, 0]);
        s.move_selected(true, &c);
        assert_eq!(s.order, [1, 2, 0]);
        s.move_selected(false, &c);
        assert_eq!(s.order, [1, 0, 2]);
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn filtering_the_list() {
        let c = cols(&["price", "name", "unit_price"]);
        let mut s = chooser(3);
        s.filter.set("PRICE");
        assert_eq!(s.rows(&c), [0, 2]);
        s.select(5, &c);
        assert_eq!(s.selected, 1);
        s.toggle_visible(&c);
        assert_eq!(s.visible, [true, true, false]);
    }
}
