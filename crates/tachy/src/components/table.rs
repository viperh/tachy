//! The table: 2-row header (name + type) and the body (spec §11.3, M1-05).
//!
//! `draw` never parses: `App` calls `Tab::prepare_frame` first, which fills
//! `Loaded::frame` with the visible rows. Placement comes from
//! `Tab::h_layout`, which navigation uses too, so what is drawn and what the
//! cursor logic believes is visible always agree.
//!
//! Row format: `[number] │ [frozen…] │ [scrolling…]`. One space separates
//! columns; dividers appear only after the gutter and after the frozen block.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::Style,
};
use tachy_core::{
    parse::display_segments,
    sort::header_indicator,
    text::{self, Align, ELLIPSIS},
};

use super::{Component, put};
use crate::{
    search_ui::{SearchState, cell_highlights},
    state::AppState,
    tab::{ColSlot, HLayout, Loaded, Phase, Tab, Viewport},
    theme::Theme,
};

/// Body text when no tab is open (D4).
pub const NO_FILE: &str = "no file open — ctrl-o to open a file";

/// Draws the active tab's table into `AppLayout::table()`: the header rows
/// on top, the body below.
#[derive(Debug, Default)]
pub struct Table;

impl Component for Table {
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()> {
        let [header, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
        let theme = &state.theme;
        let buf = frame.buffer_mut();
        buf.set_style(header, theme.surface());
        buf.set_style(body, theme.base());

        let Some(tab) = state.active_tab() else {
            centred(buf, body, NO_FILE, theme.dim());
            return Ok(());
        };
        let Some(loaded) = &tab.loaded else {
            let text = match &tab.phase {
                Phase::Spooling { .. } => "reading stdin…",
                Phase::Opening { progress }
                    if progress
                        .transcoding
                        .load(std::sync::atomic::Ordering::Relaxed) =>
                {
                    "transcoding…"
                }
                _ => "opening…",
            };
            centred(buf, body, text, theme.dim());
            return Ok(());
        };
        if loaded.source.is_empty() {
            centred(buf, body, "empty file", theme.dim());
            return Ok(());
        }
        if tab.raw_mode {
            draw_raw(buf, header, body, tab, loaded, theme);
            return Ok(());
        }

        let viewport = Viewport {
            body_height: body.height,
            table_width: area.width,
        };
        let h = tab.h_layout(viewport);
        let ctx = DrawCtx {
            tab,
            loaded,
            h: &h,
            theme,
            search: &state.find.search,
        };
        ctx.draw_header(buf, header);
        if tab.view_len_exact() && tab.view_len() == 0 {
            centred(buf, body, "0 rows", theme.dim());
            return Ok(());
        }
        ctx.draw_body(buf, body);
        Ok(())
    }
}

struct DrawCtx<'a> {
    tab: &'a Tab,
    loaded: &'a Loaded,
    h: &'a HLayout,
    theme: &'a Theme,
    search: &'a SearchState,
}

impl DrawCtx<'_> {
    fn slots(&self) -> impl Iterator<Item = &ColSlot> {
        self.h.frozen.iter().chain(&self.h.scrolling)
    }

    fn align(&self, col: usize) -> Align {
        match self.loaded.columns.get(col) {
            Some(c) if c.ty().is_numeric() => Align::Right,
            _ => Align::Left,
        }
    }

    /// The gutter divider and the frozen divider on row `y`, over `base`.
    fn draw_row_dividers(&self, buf: &mut Buffer, x0: u16, y: u16, base: Style) {
        let gutter_divider = x0 + self.h.gutter_digits + 1;
        put(
            buf,
            gutter_divider,
            y,
            "│",
            base.patch(self.theme.column_divider()),
        );
        if let Some(x) = self.h.frozen_divider {
            put(buf, x0 + x, y, "│", base.patch(self.theme.frozen_divider()));
        }
    }

    fn draw_header(&self, buf: &mut Buffer, area: Rect) {
        if area.height < 2 {
            return;
        }
        let theme = self.theme;
        let (y1, y2) = (area.y, area.y + 1);
        let surface = theme.surface();
        self.draw_row_dividers(buf, area.x, y1, surface);
        self.draw_row_dividers(buf, area.x, y2, surface);
        let view = self.tab.views.active_view();
        if !view.is_all() {
            let label = text::fit("source row", self.h.gutter_digits, Align::Right).render();
            put(
                buf,
                area.x,
                y1,
                &label,
                surface.patch(theme.gutter_header()),
            );
        }
        for slot in self.slots() {
            let col = slot.col;
            let Some(meta) = self.loaded.columns.get(col) else {
                continue;
            };
            let align = self.align(col);
            let name_style = if slot.display_idx == self.tab.cursor_col {
                theme.header_active()
            } else if view.filter_columns().contains(&col) {
                theme.header_used_by_filter()
            } else {
                theme.header()
            };
            let x = area.x + slot.x;
            // `▲` / `▼` (with the key's priority in multi-key sorts) after
            // the name of a sort key (§11.3).
            let sort = header_indicator(view.sort_keys(), col).map(|i| format!(" {i}"));
            match sort {
                Some(indicator) if slot.width > text::width(&indicator) as u16 => {
                    let iw = text::width(&indicator) as u16;
                    let fitted = text::fit(&meta.name.display, slot.width - iw, align);
                    put(buf, x, y1, &fitted.render(), surface.patch(name_style));
                    put(
                        buf,
                        x + slot.width - iw,
                        y1,
                        &indicator,
                        surface.patch(theme.sort_indicator()),
                    );
                }
                _ => {
                    let fitted = text::fit(&meta.name.display, slot.width, align);
                    put(buf, x, y1, &fitted.render(), surface.patch(name_style));
                }
            }
            let ty = text::fit(meta.ty().label(), slot.width, align).render();
            put(buf, x, y2, &ty, surface.patch(theme.header_type()));
        }
        if self.h.more_cols > 0 {
            let hint = format!(" → {} more cols", self.h.more_cols);
            let w = text::width(&hint) as u16;
            if w < area.width {
                put(
                    buf,
                    area.right() - w,
                    y1,
                    &hint,
                    surface.patch(theme.overflow_hint()),
                );
            }
        }
    }

    fn draw_body(&self, buf: &mut Buffer, area: Rect) {
        let theme = self.theme;
        let frame = &self.loaded.frame;
        let src = &self.loaded.source;
        for (i, row) in frame.rows.iter().enumerate().take(usize::from(area.height)) {
            let y = area.y + i as u16;
            let pos = frame.first + i as u64;
            let Some(row) = row else {
                continue;
            };
            let selected = pos == self.tab.cursor_row;
            let base = if selected {
                theme.row_selected()
            } else {
                theme.row(pos as usize)
            };
            buf.set_style(Rect::new(area.x, y, area.width, 1), base);

            // Gutter: 1-based view position, or the source row in
            // filtered / sorted views (§11.3).
            let number = if self.tab.views.active_view().is_all() {
                pos + 1
            } else {
                frame.ids.get(i).map_or(pos, |id| *id) + 1
            };
            let gutter_style = if selected {
                theme.gutter_selected()
            } else if row.ragged.is_ragged() {
                theme.gutter_ragged()
            } else {
                theme.gutter()
            };
            let digits = usize::from(self.h.gutter_digits);
            put(
                buf,
                area.x,
                y,
                &format!("{number:>digits$}"),
                base.patch(gutter_style),
            );
            self.draw_row_dividers(buf, area.x, y, base);

            for slot in self.slots() {
                let cursor = selected && slot.display_idx == self.tab.cursor_col;
                let style = if cursor { theme.cursor_cell() } else { base };
                let cell = Cell {
                    x: area.x + slot.x,
                    y,
                    width: slot.width,
                    align: self.align(slot.col),
                    style,
                    escaped: style.patch(theme.control_char()),
                };
                let value = row.display(src, slot.col);
                // Search and filter matches (§8.3, §12.4); the cursor cell
                // keeps its own style.
                let hl = if cursor {
                    Vec::new()
                } else {
                    cell_highlights(self.search, self.tab, slot.col, value)
                };
                cell.draw_highlighted(buf, value, &hl, theme.match_highlight());
            }
        }
    }
}

/// Raw mode (M2-04 `r`): one `line` column holding each physical line of
/// the file, the gutter showing 1-based line numbers.
fn draw_raw(buf: &mut Buffer, header: Rect, body: Rect, tab: &Tab, l: &Loaded, theme: &Theme) {
    let last = tab.top_row + u64::from(body.height);
    let digits = (last.max(1).to_string().len() as u16).div_ceil(3) * 3;
    let gutter_w = digits + 3;
    if header.width <= gutter_w {
        return;
    }
    let width = header.width - gutter_w;
    let surface = theme.surface();
    for (y, label, style) in [
        (header.y, "line", theme.header()),
        (header.y + 1, "str", theme.header_type()),
    ] {
        if y < header.bottom() {
            put(
                buf,
                header.x + digits + 1,
                y,
                "│",
                surface.patch(theme.column_divider()),
            );
            put(buf, header.x + gutter_w, y, label, surface.patch(style));
        }
    }
    for (i, line) in l
        .raw_lines
        .iter()
        .enumerate()
        .take(usize::from(body.height))
    {
        let y = body.y + i as u16;
        let n = tab.top_row + i as u64;
        let base = theme.row(n as usize);
        buf.set_style(Rect::new(body.x, y, body.width, 1), base);
        let d = usize::from(digits);
        put(
            buf,
            body.x,
            y,
            &format!("{:>d$}", n + 1),
            base.patch(theme.gutter()),
        );
        put(
            buf,
            body.x + digits + 1,
            y,
            "│",
            base.patch(theme.column_divider()),
        );
        let cell = Cell {
            x: body.x + gutter_w,
            y,
            width,
            align: Align::Left,
            style: base,
            escaped: base.patch(theme.control_char()),
        };
        cell.draw(buf, line);
    }
}

/// Where and how to draw one cell.
struct Cell {
    x: u16,
    y: u16,
    width: u16,
    align: Align,
    /// Plain text.
    style: Style,
    /// Escaped control characters (§6.2).
    escaped: Style,
}

impl Cell {
    /// [`Cell::draw`], then patches `hl` (byte ranges of `value`) with
    /// `style`. Values with escaped control characters are drawn without
    /// highlights: their display text no longer lines up with `value`.
    fn draw_highlighted(
        &self,
        buf: &mut Buffer,
        value: &str,
        hl: &[std::ops::Range<usize>],
        style: Style,
    ) {
        let visible = self.draw(buf, value);
        let Some((pad_left, shown)) = visible else {
            return;
        };
        for r in hl {
            let start = r.start.min(shown);
            let end = r.end.min(shown);
            if start >= end || !value.is_char_boundary(start) || !value.is_char_boundary(end) {
                continue;
            }
            let x = self.x + pad_left + text::width(&value[..start]) as u16;
            let w = text::width(&value[start..end]) as u16;
            let w = w.min((self.x + self.width).saturating_sub(x));
            if w > 0 {
                buf.set_style(Rect::new(x, self.y, w, 1), style);
            }
        }
    }

    /// Draws `value` fitted into the cell. A cut escaped segment keeps its
    /// style on the visible part; the `…` uses the plain style. Returns the
    /// left padding and the number of bytes of `value` shown, when `value`
    /// has no escaped segments (so highlights can be mapped onto it).
    fn draw(&self, buf: &mut Buffer, value: &str) -> Option<(u16, usize)> {
        buf.set_style(Rect::new(self.x, self.y, self.width, 1), self.style);
        let segments = display_segments(value);
        let joined: String = segments.iter().map(|s| &*s.text).collect();
        let fitted = text::fit(&joined, self.width, self.align);
        let mut x = self.x + fitted.pad_left;
        let mut remaining = fitted.text.len();
        for seg in &segments {
            if remaining == 0 {
                break;
            }
            let take = seg.text.len().min(remaining);
            let part = &seg.text[..take];
            let style = if seg.escaped {
                self.escaped
            } else {
                self.style
            };
            put(buf, x, self.y, part, style);
            x += text::width(part) as u16;
            remaining -= take;
        }
        if fitted.truncated {
            put(buf, self.x + self.width - 1, self.y, ELLIPSIS, self.style);
        }
        (!segments.iter().any(|s| s.escaped)).then_some((fitted.pad_left, fitted.text.len()))
    }
}

/// `s` centred in `area`, on its middle row.
pub fn centred(buf: &mut Buffer, area: Rect, s: &str, style: Style) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let s = text::truncate_end(s, usize::from(area.width));
    let w = text::width(&s) as u16;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - 1) / 2;
    buf.set_stringn(x, y, &s, usize::from(w), style);
}
